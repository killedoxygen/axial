use super::create::CreateVersionTagViewModel;
use axial_minecraft::{
    LoaderBuildRecord, LoaderCatalogState, LoaderComponentId, LoaderGameVersion,
    LoaderSelectionReason, compare_version_like, fetch_cached_builds,
};
use std::{cmp::Ordering, path::Path};

#[derive(Clone, Debug)]
pub(super) struct LoaderVersionPolicyInput {
    pub minecraft_version: String,
    pub stable_hint: Option<bool>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct LoaderVersionPolicyDecision {
    pub tags: Vec<CreateVersionTagViewModel>,
    pub disabled_reason: Option<String>,
    pub fresh_catalog_required: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum LoaderBuildSelectionError {
    NoBuildAvailable,
    NoCompatibleDefault { component_id: LoaderComponentId },
}

pub(super) fn evaluate_create_view_loader_version_policies(
    library_dir: &Path,
    component_id: LoaderComponentId,
    versions_catalog: &LoaderCatalogState,
    inputs: &[LoaderVersionPolicyInput],
) -> Vec<LoaderVersionPolicyDecision> {
    inputs
        .iter()
        .map(|input| {
            evaluate_loader_version_policy(library_dir, component_id, versions_catalog, input)
        })
        .collect()
}

pub(super) fn loader_version_policy_inputs(
    versions: &[LoaderGameVersion],
) -> Vec<LoaderVersionPolicyInput> {
    versions
        .iter()
        .map(|version| LoaderVersionPolicyInput {
            minecraft_version: version.id.clone(),
            stable_hint: version.stable_hint,
        })
        .collect()
}

fn evaluate_loader_version_policy(
    library_dir: &Path,
    component_id: LoaderComponentId,
    versions_catalog: &LoaderCatalogState,
    input: &LoaderVersionPolicyInput,
) -> LoaderVersionPolicyDecision {
    let mut tags = loader_version_tags(component_id, input.stable_hint);
    let compatibility =
        known_loader_minecraft_version_policy(library_dir, component_id, &input.minecraft_version);
    if compatibility.assume_unstable_default
        || compatibility
            .preferred_build
            .as_ref()
            .is_some_and(loader_build_is_unstable_default)
    {
        add_beta_tag(&mut tags);
    }
    if let Some(reason) = compatibility.disabled_reason {
        return LoaderVersionPolicyDecision {
            tags,
            disabled_reason: Some(reason),
            fresh_catalog_required: false,
        };
    }
    if loader_catalog_is_stale(versions_catalog) {
        return LoaderVersionPolicyDecision {
            tags,
            disabled_reason: None,
            fresh_catalog_required: true,
        };
    }
    LoaderVersionPolicyDecision {
        tags,
        disabled_reason: None,
        fresh_catalog_required: false,
    }
}

#[derive(Default)]
struct KnownLoaderMinecraftVersionPolicy {
    preferred_build: Option<LoaderBuildRecord>,
    disabled_reason: Option<String>,
    assume_unstable_default: bool,
}

fn known_loader_minecraft_version_policy(
    library_dir: &Path,
    component_id: LoaderComponentId,
    minecraft_version: &str,
) -> KnownLoaderMinecraftVersionPolicy {
    if component_id != LoaderComponentId::Quilt
        || !quilt_java25_minecraft_version(minecraft_version)
    {
        return KnownLoaderMinecraftVersionPolicy::default();
    }

    let Ok(Some((builds, _catalog))) =
        fetch_cached_builds(library_dir, component_id, minecraft_version)
    else {
        return KnownLoaderMinecraftVersionPolicy {
            preferred_build: None,
            disabled_reason: None,
            assume_unstable_default: true,
        };
    };
    let Some(build) = preferred_loader_build(builds) else {
        return KnownLoaderMinecraftVersionPolicy {
            preferred_build: None,
            disabled_reason: Some(no_compatible_stable_loader_message(component_id)),
            assume_unstable_default: false,
        };
    };
    KnownLoaderMinecraftVersionPolicy {
        preferred_build: Some(build),
        disabled_reason: None,
        assume_unstable_default: false,
    }
}

pub(super) fn preferred_loader_build(builds: Vec<LoaderBuildRecord>) -> Option<LoaderBuildRecord> {
    let mut compatible = builds
        .into_iter()
        .filter(|build| !loader_build_is_known_incompatible_default(build))
        .collect::<Vec<_>>();
    compatible
        .iter()
        .position(loader_build_is_stable_default)
        .map(|index| compatible.remove(index))
        .or_else(|| {
            compatible
                .iter()
                .position(loader_build_is_unstable_default)
                .map(|index| compatible.remove(index))
        })
}

pub(super) fn select_preferred_loader_build(
    component_id: LoaderComponentId,
    builds: Vec<LoaderBuildRecord>,
) -> Result<LoaderBuildRecord, LoaderBuildSelectionError> {
    let has_known_incompatible_default = builds
        .iter()
        .any(loader_build_is_known_incompatible_default);
    preferred_loader_build(builds).ok_or({
        if has_known_incompatible_default {
            LoaderBuildSelectionError::NoCompatibleDefault { component_id }
        } else {
            LoaderBuildSelectionError::NoBuildAvailable
        }
    })
}

pub(super) fn loader_build_is_known_incompatible_default(build: &LoaderBuildRecord) -> bool {
    build.component_id == LoaderComponentId::Quilt
        && quilt_java25_minecraft_version(&build.minecraft_version)
        && quilt_loader_version_is_before_java25_support(&build.loader_version)
}

pub(super) fn no_compatible_stable_loader_message(component_id: LoaderComponentId) -> String {
    format!(
        "No stable compatible {} loader is available for this Minecraft version.",
        component_id.display_name()
    )
}

pub(super) fn loader_catalog_is_stale(catalog: &LoaderCatalogState) -> bool {
    catalog.availability.stale || !catalog.availability.fresh
}

pub(super) fn stale_loader_version_catalog_message() -> String {
    "Loader catalog needs a fresh provider check before this version can be installed.".to_string()
}

fn loader_version_tags(
    component_id: LoaderComponentId,
    stable_hint: Option<bool>,
) -> Vec<CreateVersionTagViewModel> {
    let mut tags = Vec::new();
    if matches!(
        component_id,
        LoaderComponentId::Forge | LoaderComponentId::NeoForge
    ) && stable_hint == Some(false)
    {
        add_beta_tag(&mut tags);
    }
    tags
}

fn add_beta_tag(tags: &mut Vec<CreateVersionTagViewModel>) {
    if tags.iter().any(|tag| tag.id == "beta") {
        return;
    }
    tags.push(CreateVersionTagViewModel {
        id: "beta".to_string(),
        label: "Beta".to_string(),
    });
}

fn loader_build_is_stable_default(build: &LoaderBuildRecord) -> bool {
    matches!(
        build.build_meta.selection.reason,
        LoaderSelectionReason::Recommended
            | LoaderSelectionReason::LatestStable
            | LoaderSelectionReason::Stable
            | LoaderSelectionReason::Unlabeled
    )
}

pub(super) fn loader_build_is_unstable_default(build: &LoaderBuildRecord) -> bool {
    matches!(
        build.build_meta.selection.reason,
        LoaderSelectionReason::Latest
            | LoaderSelectionReason::LatestUnstable
            | LoaderSelectionReason::Unstable
    )
}

fn quilt_java25_minecraft_version(minecraft_version: &str) -> bool {
    let value = minecraft_version.trim();
    value == "26" || value.starts_with("26.")
}

fn quilt_loader_version_is_before_java25_support(loader_version: &str) -> bool {
    let value = loader_version.trim();
    compare_version_like(value, "0.30.0") == Ordering::Less && !value.starts_with("0.30.")
}

#[cfg(test)]
mod tests {
    use super::*;
    use axial_minecraft::{
        LoaderArtifactKind, LoaderBuildMetadata, LoaderInstallStrategy, LoaderInstallability,
        LoaderSelectionMeta, LoaderSelectionSource, build_id_for, installed_version_id_for,
        loaders::{LoaderInstallSource, types::LoaderBuildSubjectKind},
    };

    #[test]
    fn preferred_loader_build_prefers_stable_and_falls_back_to_unstable() {
        let component_id = LoaderComponentId::NeoForge;
        let mut unstable = loader_build(component_id, "26.2", "26.2.0.3-beta", 600);
        unstable.build_meta.selection.reason = LoaderSelectionReason::Unstable;
        let stable = loader_build(component_id, "26.2", "26.2.0.4", 800);

        let preferred =
            preferred_loader_build(vec![unstable.clone(), stable.clone()]).expect("stable build");
        assert_eq!(preferred.build_id, stable.build_id);

        let fallback = preferred_loader_build(vec![unstable.clone()]).expect("unstable fallback");
        assert_eq!(fallback.build_id, unstable.build_id);
    }

    #[test]
    fn select_preferred_loader_build_reports_quilt_java25_without_compatible_default() {
        let component_id = LoaderComponentId::Quilt;
        let incompatible = loader_build(component_id, "26.1.2", "0.29.2", 700);

        let error = select_preferred_loader_build(component_id, vec![incompatible])
            .expect_err("incompatible Quilt default should fail");

        assert_eq!(
            error,
            LoaderBuildSelectionError::NoCompatibleDefault { component_id }
        );
    }

    #[test]
    fn select_preferred_loader_build_reports_provider_filtered_catalog_as_unavailable() {
        let error = select_preferred_loader_build(LoaderComponentId::Fabric, Vec::new())
            .expect_err("empty provider-compatible catalog should fail");

        assert_eq!(error, LoaderBuildSelectionError::NoBuildAvailable);
    }

    #[test]
    fn preferred_loader_build_skips_incompatible_quilt_java25_and_uses_compatible_beta() {
        let component_id = LoaderComponentId::Quilt;
        let incompatible = loader_build(component_id, "26.1.2", "0.29.2", 700);
        let mut beta = loader_build(component_id, "26.1.2", "0.30.0-beta.8", 600);
        beta.build_meta.selection.reason = LoaderSelectionReason::Unstable;
        beta.build_meta.selection.source = LoaderSelectionSource::ExplicitVersionLabel;

        let preferred =
            preferred_loader_build(vec![incompatible, beta.clone()]).expect("compatible fallback");

        assert_eq!(preferred.build_id, beta.build_id);
    }

    fn loader_build(
        component_id: LoaderComponentId,
        minecraft_version: &str,
        loader_version: &str,
        default_rank: i32,
    ) -> LoaderBuildRecord {
        let build_id = build_id_for(component_id, minecraft_version, loader_version);
        let version_id = installed_version_id_for(component_id, minecraft_version, loader_version)
            .expect("valid test loader identity");
        LoaderBuildRecord {
            subject_kind: LoaderBuildSubjectKind::LoaderBuild,
            component_id,
            component_name: component_id.display_name().to_string(),
            build_id,
            minecraft_version: minecraft_version.to_string(),
            loader_version: loader_version.to_string(),
            version_id,
            build_meta: LoaderBuildMetadata {
                selection: LoaderSelectionMeta {
                    default_rank,
                    reason: LoaderSelectionReason::Recommended,
                    source: LoaderSelectionSource::ExplicitApiFlag,
                },
                ..LoaderBuildMetadata::default()
            },
            strategy: LoaderInstallStrategy::FabricProfile,
            artifact_kind: LoaderArtifactKind::ProfileJson,
            installability: LoaderInstallability::Installable,
            install_source: LoaderInstallSource::ProfileJson {
                url: "https://example.invalid/loader-profile.json".to_string(),
            },
        }
    }
}
