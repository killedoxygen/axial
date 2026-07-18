use crate::state::AppState;
use axial_content::{
    CanonicalId, ContentKind, ContentManifest, ContentResolution, DependencyKind, ProviderId,
    ResolutionReason, ResolutionSelection, ResolutionTarget, ResolvedContentItem, resolve_content,
};
use axial_performance::{
    CompositionPlan, ManagedArtifactPin, ManagedArtifactRole, ManagedCompositionInstallPlan,
    ManagedDependencyEdge,
};
use std::collections::{BTreeMap, BTreeSet};
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub(super) enum ManagedPlanResolutionError {
    #[error("managed composition declares an invalid root set")]
    InvalidRootSet,
    #[error("managed content resolution failed")]
    ResolutionFailed,
    #[error("managed content resolution reported a conflict")]
    ResolutionConflict,
    #[error("managed content resolution returned an invalid artifact graph")]
    InvalidArtifactGraph,
    #[error("managed content resolution returned an invalid dependency graph")]
    InvalidDependencyGraph,
    #[error("managed content resolution could not be sealed")]
    SealRejected,
}

/// Resolves the provider graph before any managed-composition effect starts.
/// Provider diagnostics deliberately stop at this internal, static error boundary.
pub(super) async fn resolve_managed_install_plan(
    state: &AppState,
    declarative: CompositionPlan,
    game_version: &str,
    loader: &str,
) -> Result<ManagedCompositionInstallPlan, ManagedPlanResolutionError> {
    let root_ids = declarative_root_ids(&declarative)?;
    if root_ids.is_empty() {
        return seal_managed_content_resolution(
            declarative,
            game_version,
            loader,
            ContentResolution {
                items: Vec::new(),
                conflicts: Vec::new(),
            },
        );
    }

    let selections = root_ids
        .into_iter()
        .map(|project_id| ResolutionSelection {
            canonical_id: CanonicalId::for_project(ProviderId::Modrinth, &project_id)
                .as_str()
                .to_string(),
            kind: ContentKind::Mod,
            version_id: None,
        })
        .collect::<Vec<_>>();
    let target = ResolutionTarget {
        game_dir: None,
        loader: loader.to_string(),
        game_version: game_version.to_string(),
        supports_mods: !loader.is_empty() && loader != "vanilla",
    };
    let resolution = resolve_content(
        state.content(),
        &target,
        &selections,
        &ContentManifest::default(),
    )
    .await
    .map_err(|_| ManagedPlanResolutionError::ResolutionFailed)?;

    seal_managed_content_resolution(declarative, game_version, loader, resolution)
}

pub(super) fn seal_managed_content_resolution(
    declarative: CompositionPlan,
    game_version: &str,
    loader: &str,
    resolution: ContentResolution,
) -> Result<ManagedCompositionInstallPlan, ManagedPlanResolutionError> {
    let root_ids = declarative_root_ids(&declarative)?;
    if !resolution.conflicts.is_empty() {
        return Err(ManagedPlanResolutionError::ResolutionConflict);
    }

    let mut selected_ids = BTreeSet::new();
    let mut versions_by_project = BTreeMap::new();
    let mut projects_by_version = BTreeMap::new();
    let mut pins = Vec::with_capacity(resolution.items.len());
    for item in &resolution.items {
        validate_modrinth_mod(item)?;
        if versions_by_project
            .insert(item.project_id.clone(), item.version_id.clone())
            .is_some()
        {
            return Err(ManagedPlanResolutionError::InvalidArtifactGraph);
        }
        if projects_by_version
            .insert(item.version_id.clone(), item.project_id.clone())
            .is_some()
        {
            return Err(ManagedPlanResolutionError::InvalidDependencyGraph);
        }

        let role = match item.reason {
            ResolutionReason::Selected => {
                selected_ids.insert(item.project_id.clone());
                ManagedArtifactRole::Root
            }
            ResolutionReason::Dependency => ManagedArtifactRole::RequiredDependency,
        };
        let size = item
            .file
            .size
            .ok_or(ManagedPlanResolutionError::InvalidArtifactGraph)?;
        let sha512 = item
            .file
            .sha512
            .as_deref()
            .ok_or(ManagedPlanResolutionError::InvalidArtifactGraph)?;
        pins.push(
            ManagedArtifactPin::new(
                &item.project_id,
                &item.version_id,
                &item.file.filename,
                &item.file.url,
                size,
                sha512,
                role,
            )
            .map_err(|_| ManagedPlanResolutionError::InvalidArtifactGraph)?,
        );
    }
    if selected_ids != root_ids {
        return Err(ManagedPlanResolutionError::InvalidRootSet);
    }

    let mut edges = Vec::new();
    for item in &resolution.items {
        for dependency in &item.dependencies {
            match dependency.kind {
                DependencyKind::Required => {
                    let child_project_id = match dependency.project_id.as_deref() {
                        Some(project_id) => project_id,
                        None => dependency
                            .version_id
                            .as_ref()
                            .and_then(|version_id| projects_by_version.get(version_id))
                            .map(String::as_str)
                            .ok_or(ManagedPlanResolutionError::InvalidDependencyGraph)?,
                    };
                    let child_version_id = versions_by_project
                        .get(child_project_id)
                        .ok_or(ManagedPlanResolutionError::InvalidDependencyGraph)?;
                    if dependency
                        .version_id
                        .as_deref()
                        .is_some_and(|required| required != child_version_id)
                    {
                        return Err(ManagedPlanResolutionError::InvalidDependencyGraph);
                    }
                    edges.push(
                        ManagedDependencyEdge::new(
                            &item.project_id,
                            child_project_id,
                            child_version_id,
                        )
                        .map_err(|_| ManagedPlanResolutionError::InvalidDependencyGraph)?,
                    );
                }
                DependencyKind::Incompatible => {
                    let admitted = match (
                        dependency.project_id.as_deref(),
                        dependency.version_id.as_deref(),
                    ) {
                        (Some(project_id), Some(version_id)) => versions_by_project
                            .get(project_id)
                            .is_some_and(|resolved| resolved == version_id),
                        (Some(project_id), None) => versions_by_project.contains_key(project_id),
                        (None, Some(version_id)) => projects_by_version.contains_key(version_id),
                        (None, None) => {
                            return Err(ManagedPlanResolutionError::InvalidDependencyGraph);
                        }
                    };
                    if admitted {
                        return Err(ManagedPlanResolutionError::ResolutionConflict);
                    }
                }
                DependencyKind::Optional | DependencyKind::Embedded => {}
            }
        }
    }

    ManagedCompositionInstallPlan::seal(declarative, game_version, loader, pins, edges)
        .map_err(|_| ManagedPlanResolutionError::SealRejected)
}

fn declarative_root_ids(
    declarative: &CompositionPlan,
) -> Result<BTreeSet<String>, ManagedPlanResolutionError> {
    let root_ids = declarative
        .mods
        .iter()
        .map(|managed_mod| managed_mod.project_id.clone())
        .collect::<BTreeSet<_>>();
    if root_ids.len() != declarative.mods.len() {
        Err(ManagedPlanResolutionError::InvalidRootSet)
    } else {
        Ok(root_ids)
    }
}

fn validate_modrinth_mod(item: &ResolvedContentItem) -> Result<(), ManagedPlanResolutionError> {
    let expected = CanonicalId::for_project(ProviderId::Modrinth, &item.project_id);
    if item.provider != ProviderId::Modrinth
        || item.kind != ContentKind::Mod
        || item.canonical_id != expected
        || item.already_installed
        || item.update
    {
        Err(ManagedPlanResolutionError::InvalidArtifactGraph)
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axial_content::{ContentDependency, FileRef, ResolutionConflict, ResolutionConflictReason};
    use axial_performance::types::{
        CompositionTier, ManagedMod, ModCondition, PerformanceMode, VersionFamily,
    };

    const ROOT_A: &str = "AANobbMI";
    const ROOT_B: &str = "gvQqBUqZ";
    const DEP_A: &str = "P7dR8mSH";
    const OUTSIDE: &str = "9s6osm5g";
    const VERSION_A: &str = "NFkjnzWE";
    const VERSION_B: &str = "1234abcd";
    const VERSION_DEP: &str = "8765dcba";
    const SHA512: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn managed_mod(project_id: &str) -> ManagedMod {
        ManagedMod {
            artifact_id: format!("artifact-{project_id}"),
            project_id: project_id.to_string(),
            slug: String::new(),
            name: project_id.to_string(),
            condition: ModCondition::Always,
            version_range: String::new(),
            exact_game_versions: Vec::new(),
            hardware_req: None,
            mutual_exclusions: Vec::new(),
        }
    }

    fn declarative(roots: &[&str]) -> CompositionPlan {
        CompositionPlan {
            composition_id: "managed-test".to_string(),
            family: VersionFamily::A,
            loader: "fabric".to_string(),
            mode: PerformanceMode::Managed,
            tier: CompositionTier::Core,
            mods: roots.iter().map(|root| managed_mod(root)).collect(),
            jvm_preset: String::new(),
            fallback_chain: Vec::new(),
            warnings: Vec::new(),
            fallback_reason: String::new(),
        }
    }

    fn item(
        project_id: &str,
        version_id: &str,
        reason: ResolutionReason,
        dependencies: Vec<ContentDependency>,
    ) -> ResolvedContentItem {
        ResolvedContentItem {
            canonical_id: CanonicalId::for_project(ProviderId::Modrinth, project_id),
            provider: ProviderId::Modrinth,
            project_id: project_id.to_string(),
            kind: ContentKind::Mod,
            version_id: version_id.to_string(),
            version_number: "1.0.0".to_string(),
            title: project_id.to_string(),
            file: FileRef {
                url: format!("https://cdn.example.test/{project_id}.jar"),
                filename: format!("{project_id}.jar"),
                sha1: None,
                sha512: Some(SHA512.to_string()),
                size: Some(1024),
                primary: true,
            },
            dependencies,
            reason,
            already_installed: false,
            update: false,
        }
    }

    fn dependency(
        project_id: Option<&str>,
        version_id: Option<&str>,
        kind: DependencyKind,
    ) -> ContentDependency {
        ContentDependency {
            project_id: project_id.map(str::to_string),
            version_id: version_id.map(str::to_string),
            kind,
        }
    }

    fn resolution(items: Vec<ResolvedContentItem>) -> ContentResolution {
        ContentResolution {
            items,
            conflicts: Vec::new(),
        }
    }

    #[test]
    fn seals_roots_and_required_dependency_closure() {
        let plan = seal_managed_content_resolution(
            declarative(&[ROOT_A, ROOT_B]),
            "1.21.11",
            "fabric",
            resolution(vec![
                item(
                    ROOT_A,
                    VERSION_A,
                    ResolutionReason::Selected,
                    vec![
                        dependency(Some(DEP_A), None, DependencyKind::Required),
                        dependency(Some(ROOT_B), None, DependencyKind::Optional),
                        dependency(Some(ROOT_B), None, DependencyKind::Embedded),
                        dependency(Some(OUTSIDE), None, DependencyKind::Incompatible),
                    ],
                ),
                item(ROOT_B, VERSION_B, ResolutionReason::Selected, Vec::new()),
                item(DEP_A, VERSION_DEP, ResolutionReason::Dependency, Vec::new()),
            ]),
        )
        .expect("sealed plan");

        assert_eq!(plan.pins().len(), 3);
        assert_eq!(plan.edges().len(), 1);
        assert_eq!(plan.edges()[0].parent_project_id(), ROOT_A);
        assert_eq!(plan.edges()[0].child_project_id(), DEP_A);
        assert_eq!(plan.edges()[0].child_version_id(), VERSION_DEP);
        assert_eq!(plan.aggregate_bytes(), 3072);
        assert_eq!(
            plan.pins()
                .iter()
                .find(|pin| pin.project_id() == DEP_A)
                .expect("dependency pin")
                .role(),
            ManagedArtifactRole::RequiredDependency
        );
    }

    #[test]
    fn resolves_version_only_required_dependency_to_unique_exact_pin() {
        let plan = seal_managed_content_resolution(
            declarative(&[ROOT_A]),
            "1.21.11",
            "fabric",
            resolution(vec![
                item(
                    ROOT_A,
                    VERSION_A,
                    ResolutionReason::Selected,
                    vec![dependency(
                        None,
                        Some(VERSION_DEP),
                        DependencyKind::Required,
                    )],
                ),
                item(DEP_A, VERSION_DEP, ResolutionReason::Dependency, Vec::new()),
            ]),
        )
        .expect("sealed plan");

        assert_eq!(plan.edges()[0].child_project_id(), DEP_A);
        assert_eq!(plan.edges()[0].child_version_id(), VERSION_DEP);
    }

    #[test]
    fn rejects_resolution_conflicts_without_reading_provider_copy() {
        let result = seal_managed_content_resolution(
            declarative(&[ROOT_A]),
            "1.21.11",
            "fabric",
            ContentResolution {
                items: Vec::new(),
                conflicts: vec![ResolutionConflict {
                    canonical_id: Some(CanonicalId::for_project(ProviderId::Modrinth, ROOT_A)),
                    subject_title: Some("provider-authored secret".to_string()),
                    reason: ResolutionConflictReason::NoCompatibleVersion,
                }],
            },
        );

        assert_eq!(
            result.expect_err("conflict must fail"),
            ManagedPlanResolutionError::ResolutionConflict
        );
        assert_eq!(
            ManagedPlanResolutionError::ResolutionConflict.to_string(),
            "managed content resolution reported a conflict"
        );
    }

    #[test]
    fn rejects_missing_duplicate_and_dependency_marked_roots() {
        for invalid in [
            resolution(Vec::new()),
            resolution(vec![
                item(ROOT_A, VERSION_A, ResolutionReason::Selected, Vec::new()),
                item(ROOT_A, VERSION_B, ResolutionReason::Selected, Vec::new()),
            ]),
            resolution(vec![item(
                ROOT_A,
                VERSION_A,
                ResolutionReason::Dependency,
                Vec::new(),
            )]),
        ] {
            assert!(
                seal_managed_content_resolution(
                    declarative(&[ROOT_A]),
                    "1.21.11",
                    "fabric",
                    invalid,
                )
                .is_err()
            );
        }
    }

    #[test]
    fn rejects_duplicate_declarative_roots() {
        assert_eq!(
            seal_managed_content_resolution(
                declarative(&[ROOT_A, ROOT_A]),
                "1.21.11",
                "fabric",
                resolution(Vec::new()),
            )
            .expect_err("duplicate root must fail"),
            ManagedPlanResolutionError::InvalidRootSet
        );
    }

    #[test]
    fn rejects_non_modrinth_or_identity_mismatched_artifacts() {
        let mut wrong_kind = item(ROOT_A, VERSION_A, ResolutionReason::Selected, Vec::new());
        wrong_kind.kind = ContentKind::ResourcePack;
        let mut wrong_identity = item(ROOT_A, VERSION_A, ResolutionReason::Selected, Vec::new());
        wrong_identity.canonical_id = CanonicalId::for_project(ProviderId::Modrinth, ROOT_B);

        for invalid in [wrong_kind, wrong_identity] {
            assert_eq!(
                seal_managed_content_resolution(
                    declarative(&[ROOT_A]),
                    "1.21.11",
                    "fabric",
                    resolution(vec![invalid]),
                )
                .expect_err("invalid identity must fail"),
                ManagedPlanResolutionError::InvalidArtifactGraph
            );
        }
    }

    #[test]
    fn rejects_missing_or_noncanonical_artifact_authority() {
        let mut missing_size = item(ROOT_A, VERSION_A, ResolutionReason::Selected, Vec::new());
        missing_size.file.size = None;
        let mut missing_hash = item(ROOT_A, VERSION_A, ResolutionReason::Selected, Vec::new());
        missing_hash.file.sha512 = None;
        let mut invalid_url = item(ROOT_A, VERSION_A, ResolutionReason::Selected, Vec::new());
        invalid_url.file.url = "http://cdn.example.test/root.jar".to_string();
        let mut invalid_filename = item(ROOT_A, VERSION_A, ResolutionReason::Selected, Vec::new());
        invalid_filename.file.filename = "../root.jar".to_string();

        for invalid in [missing_size, missing_hash, invalid_url, invalid_filename] {
            assert_eq!(
                seal_managed_content_resolution(
                    declarative(&[ROOT_A]),
                    "1.21.11",
                    "fabric",
                    resolution(vec![invalid]),
                )
                .expect_err("invalid artifact authority must fail"),
                ManagedPlanResolutionError::InvalidArtifactGraph
            );
        }
    }

    #[test]
    fn rejects_non_draft_install_state_but_accepts_resolved_single_file_authority() {
        let mut already_installed = item(ROOT_A, VERSION_A, ResolutionReason::Selected, Vec::new());
        already_installed.already_installed = true;
        let mut update = item(ROOT_A, VERSION_A, ResolutionReason::Selected, Vec::new());
        update.update = true;

        for invalid in [already_installed, update] {
            assert_eq!(
                seal_managed_content_resolution(
                    declarative(&[ROOT_A]),
                    "1.21.11",
                    "fabric",
                    resolution(vec![invalid]),
                )
                .expect_err("draft state mismatch must fail"),
                ManagedPlanResolutionError::InvalidArtifactGraph
            );
        }

        let mut sole_unmarked_file =
            item(ROOT_A, VERSION_A, ResolutionReason::Selected, Vec::new());
        sole_unmarked_file.file.primary = false;
        seal_managed_content_resolution(
            declarative(&[ROOT_A]),
            "1.21.11",
            "fabric",
            resolution(vec![sole_unmarked_file]),
        )
        .expect("Content already selected the sole unmarked file as install authority");
    }

    #[test]
    fn treats_resolved_incompatibilities_as_admission_failures_not_edges() {
        let result = seal_managed_content_resolution(
            declarative(&[ROOT_A, ROOT_B]),
            "1.21.11",
            "fabric",
            resolution(vec![
                item(
                    ROOT_A,
                    VERSION_A,
                    ResolutionReason::Selected,
                    vec![dependency(
                        Some(ROOT_B),
                        Some(VERSION_B),
                        DependencyKind::Incompatible,
                    )],
                ),
                item(ROOT_B, VERSION_B, ResolutionReason::Selected, Vec::new()),
            ]),
        );

        assert_eq!(
            result.expect_err("incompatible graph must not be sealed"),
            ManagedPlanResolutionError::ResolutionConflict
        );
    }

    #[test]
    fn rejects_missing_ambiguous_and_mismatched_required_dependencies() {
        let cases = [
            vec![item(
                ROOT_A,
                VERSION_A,
                ResolutionReason::Selected,
                vec![dependency(None, None, DependencyKind::Required)],
            )],
            vec![item(
                ROOT_A,
                VERSION_A,
                ResolutionReason::Selected,
                vec![dependency(Some(DEP_A), None, DependencyKind::Required)],
            )],
            vec![
                item(
                    ROOT_A,
                    VERSION_A,
                    ResolutionReason::Selected,
                    vec![dependency(
                        Some(DEP_A),
                        Some(VERSION_B),
                        DependencyKind::Required,
                    )],
                ),
                item(DEP_A, VERSION_DEP, ResolutionReason::Dependency, Vec::new()),
            ],
            vec![
                item(
                    ROOT_A,
                    VERSION_A,
                    ResolutionReason::Selected,
                    vec![dependency(
                        None,
                        Some(VERSION_DEP),
                        DependencyKind::Required,
                    )],
                ),
                item(DEP_A, VERSION_DEP, ResolutionReason::Dependency, Vec::new()),
                item(
                    ROOT_B,
                    VERSION_DEP,
                    ResolutionReason::Dependency,
                    Vec::new(),
                ),
            ],
        ];

        for items in cases {
            assert_eq!(
                seal_managed_content_resolution(
                    declarative(&[ROOT_A]),
                    "1.21.11",
                    "fabric",
                    resolution(items),
                )
                .expect_err("invalid dependency must fail"),
                ManagedPlanResolutionError::InvalidDependencyGraph
            );
        }
    }

    #[test]
    fn seals_an_empty_declarative_graph_without_provider_items() {
        let plan = seal_managed_content_resolution(
            declarative(&[]),
            "1.21.11",
            "fabric",
            resolution(Vec::new()),
        )
        .expect("empty sealed plan");

        assert!(plan.pins().is_empty());
        assert!(plan.edges().is_empty());
        assert_eq!(plan.aggregate_bytes(), 0);
    }
}
