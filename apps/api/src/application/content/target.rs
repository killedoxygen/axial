//! Where a plan is resolved against. Either an instance that exists, or a draft
//! one the user has not created yet — the resolver treats the latter as an empty
//! instance with a known loader and Minecraft version, so both go through the
//! same dependency and conflict pass.

use super::{ContentApiError, json_error};
use crate::state::AppState;
use axial_content::{ContentKind, LoaderGameFilter};
use axum::http::StatusCode;
use serde::Deserialize;
use std::path::PathBuf;

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TargetRef {
    Instance {
        instance_id: String,
    },
    /// An instance the user is about to create. Resolving against it previews
    /// what the install would do before anything exists on disk.
    Draft {
        #[serde(default)]
        loader: Option<String>,
        game_version: String,
    },
}

pub struct ResolveTarget {
    /// `None` for a draft: there is nowhere to read a manifest from or write to.
    pub game_dir: Option<PathBuf>,
    pub loader: String,
    pub game_version: String,
    pub supports_mods: bool,
}

impl ResolveTarget {
    /// The version filter for a given kind. Only mods and modpacks are tagged
    /// with the instance's loader upstream, so narrowing a resource pack or a
    /// shader by it would match nothing.
    pub fn filter_for(&self, kind: ContentKind) -> LoaderGameFilter {
        LoaderGameFilter {
            loader: (kind.filters_by_loader() && self.supports_mods).then(|| self.loader.clone()),
            game_version: Some(self.game_version.clone()).filter(|value| !value.is_empty()),
        }
    }
}

pub async fn resolve_target(
    state: &AppState,
    target: &TargetRef,
) -> Result<ResolveTarget, ContentApiError> {
    match target {
        TargetRef::Instance { instance_id } => instance_target(state, instance_id).await,
        TargetRef::Draft {
            loader,
            game_version,
        } => {
            let game_version = game_version.trim();
            if game_version.is_empty() {
                return Err(json_error(
                    StatusCode::BAD_REQUEST,
                    "a Minecraft version is required",
                ));
            }
            let loader = loader
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty() && *value != "vanilla")
                .unwrap_or_default()
                .to_string();
            Ok(ResolveTarget {
                game_dir: None,
                supports_mods: !loader.is_empty(),
                loader,
                game_version: game_version.to_string(),
            })
        }
    }
}

pub async fn instance_target(
    state: &AppState,
    instance_id: &str,
) -> Result<ResolveTarget, ContentApiError> {
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

    Ok(ResolveTarget {
        game_dir: Some(state.instances().game_dir(&instance.id)),
        loader: display.loader_key,
        game_version: display.minecraft_label,
        supports_mods: display.supports_mods,
    })
}

pub fn require_instance_game_dir(
    state: &AppState,
    instance_id: &str,
) -> Result<PathBuf, ContentApiError> {
    let instance = state
        .instances()
        .get(instance_id)
        .ok_or_else(|| json_error(StatusCode::NOT_FOUND, "instance not found"))?;
    Ok(state.instances().game_dir(&instance.id))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(loader: &str, supports_mods: bool) -> ResolveTarget {
        ResolveTarget {
            game_dir: None,
            loader: loader.to_string(),
            game_version: "1.21.6".to_string(),
            supports_mods,
        }
    }

    #[test]
    fn mods_filter_by_loader_but_packs_do_not() {
        let fabric = target("fabric", true);

        let mods = fabric.filter_for(ContentKind::Mod);
        assert_eq!(mods.loader.as_deref(), Some("fabric"));
        assert_eq!(mods.game_version.as_deref(), Some("1.21.6"));

        for kind in [ContentKind::ResourcePack, ContentKind::ShaderPack] {
            let filter = fabric.filter_for(kind);
            assert_eq!(filter.loader, None, "{kind:?} must not filter by loader");
            assert_eq!(filter.game_version.as_deref(), Some("1.21.6"));
        }
    }

    #[test]
    fn a_vanilla_target_never_filters_by_loader() {
        let vanilla = target("vanilla", false);
        assert_eq!(vanilla.filter_for(ContentKind::Mod).loader, None);
        assert_eq!(vanilla.filter_for(ContentKind::ResourcePack).loader, None);
    }
}
