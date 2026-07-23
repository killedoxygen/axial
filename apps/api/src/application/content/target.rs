//! Where a plan is resolved against. Either an instance that exists, or a draft
//! one the user has not created yet — the resolver treats the latter as an empty
//! instance with a known loader and Minecraft version, so both go through the
//! same dependency and conflict pass.

use super::{ContentApiError, json_error};
use crate::state::AppState;
use axial_content::ResolutionTarget;
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Application-only query target. The resolver identity is path-free; the
/// optional path exists only for ambient read surfaces and deferred modpacks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolveTarget {
    resolution: ResolutionTarget,
    game_dir: Option<PathBuf>,
}

impl ResolveTarget {
    pub fn resolution(&self) -> &ResolutionTarget {
        &self.resolution
    }

    pub fn game_dir(&self) -> Option<&Path> {
        self.game_dir.as_deref()
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
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

fn target_from_stored_metadata(
    loader: &str,
    game_version: &str,
    game_dir: PathBuf,
) -> Option<ResolveTarget> {
    let game_version = game_version.trim();
    if game_version.is_empty() {
        return None;
    }
    let loader = loader.trim();
    Some(ResolveTarget {
        resolution: ResolutionTarget {
            loader: loader.to_string(),
            game_version: game_version.to_string(),
            supports_mods: !loader.is_empty() && loader != "vanilla",
        },
        game_dir: Some(game_dir),
    })
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
                resolution: ResolutionTarget {
                    supports_mods: !loader.is_empty(),
                    loader,
                    game_version: game_version.to_string(),
                },
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
    if let Some(target) = target_from_stored_metadata(
        &instance.loader_key,
        &instance.minecraft_version,
        state.instances().game_dir(&instance.id),
    ) {
        return Ok(target);
    }
    Err(json_error(
        StatusCode::CONFLICT,
        "instance content identity is incomplete; recreate the instance before changing content",
    ))
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
    use axial_content::ContentKind;

    fn target(loader: &str, supports_mods: bool) -> ResolveTarget {
        ResolveTarget {
            game_dir: None,
            resolution: ResolutionTarget {
                loader: loader.to_string(),
                game_version: "1.21.6".to_string(),
                supports_mods,
            },
        }
    }

    #[test]
    fn mods_filter_by_loader_but_packs_do_not() {
        let fabric = target("fabric", true);

        let mods = fabric.resolution().filter_for(ContentKind::Mod);
        assert_eq!(mods.loader.as_deref(), Some("fabric"));
        assert_eq!(mods.game_version.as_deref(), Some("1.21.6"));

        for kind in [ContentKind::ResourcePack, ContentKind::ShaderPack] {
            let filter = fabric.resolution().filter_for(kind);
            assert_eq!(filter.loader, None, "{kind:?} must not filter by loader");
            assert_eq!(filter.game_version.as_deref(), Some("1.21.6"));
        }
    }

    #[test]
    fn a_vanilla_target_never_filters_by_loader() {
        let vanilla = target("vanilla", false);
        assert_eq!(
            vanilla.resolution().filter_for(ContentKind::Mod).loader,
            None
        );
        assert_eq!(
            vanilla
                .resolution()
                .filter_for(ContentKind::ResourcePack)
                .loader,
            None
        );
    }

    #[test]
    fn stored_loader_metadata_is_usable_before_the_loader_finishes_installing() {
        let target = target_from_stored_metadata(" forge ", " 26.2 ", PathBuf::from("/instance"))
            .expect("stored target");

        assert_eq!(target.resolution().loader, "forge");
        assert_eq!(target.resolution().game_version, "26.2");
        assert!(target.resolution().supports_mods);
    }
}
