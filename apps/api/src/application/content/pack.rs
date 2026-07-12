//! Installing a modpack into an instance. The frontend creates the instance
//! first — the pack's loader and Minecraft version are enough to address the
//! create API — and then hands the empty instance here to be filled.
//!
//! Pack files carry sha512 hashes in the index, so once they are on disk we can
//! ask the provider what they are in one batch and record real provenance for
//! every mod, rather than leaving a pack-shaped hole in the manifest.

use super::resolve::pick_version;
use super::target::instance_target;
use super::{ContentApiError, content_error_response, json_error};
use crate::state::AppState;
use axial_content::{
    CanonicalId, ContentKind, ContentManifest, EntrySource, FileRef, ManifestEntry, ProviderId,
    install_pack,
};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Deserialize)]
pub struct ModpackInstallRequest {
    pub instance_id: String,
    pub canonical_id: String,
    #[serde(default)]
    pub version_id: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ModpackInstallResponse {
    pub instance_id: String,
    pub name: String,
    pub version: String,
    pub minecraft: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub loader: Option<String>,
    pub file_count: usize,
    pub overrides_applied: usize,
    pub identified_count: usize,
    /// Set when the pack wants a different loader or Minecraft version than the
    /// instance has. The files still install; the game may not start.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mismatch: Option<String>,
}

/// What a pack needs, so the caller can create a matching instance before
/// installing it.
#[derive(Debug, Serialize)]
pub struct ModpackTarget {
    pub canonical_id: CanonicalId,
    pub version_id: String,
    pub name: String,
    pub minecraft: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub loader: Option<String>,
    pub loader_label: String,
    /// Ready to POST to `/instances`.
    pub selection_id: String,
}

/// Read a pack version's declared loader and Minecraft version straight from the
/// provider metadata — no download needed, so the create step stays instant.
pub async fn modpack_target(
    state: &AppState,
    canonical_id: &str,
    version_id: Option<&str>,
) -> Result<ModpackTarget, ContentApiError> {
    let id = CanonicalId(canonical_id.to_string());
    let detail = state
        .content()
        .detail(&id)
        .await
        .map_err(content_error_response)?;
    if detail.content.kind != ContentKind::Modpack {
        return Err(json_error(StatusCode::BAD_REQUEST, "this is not a modpack"));
    }

    let version = pick_version(&detail.versions, version_id).ok_or_else(|| {
        json_error(
            StatusCode::NOT_FOUND,
            "this modpack has no installable version",
        )
    })?;

    let minecraft = version
        .game_versions
        .iter()
        .find(|value| is_release_version(value))
        .or_else(|| version.game_versions.first())
        .cloned()
        .ok_or_else(|| {
            json_error(
                StatusCode::BAD_REQUEST,
                "this modpack does not say which Minecraft version it needs",
            )
        })?;

    let loader = version
        .loaders
        .iter()
        .find(|value| axial_minecraft::LoaderComponentId::parse(value).is_some())
        .and_then(|value| axial_minecraft::LoaderComponentId::parse(value));

    Ok(ModpackTarget {
        canonical_id: id,
        version_id: version.id.clone(),
        name: detail.content.title.clone(),
        selection_id: match loader {
            Some(id) => format!("loader_version|{}|{}", id.short_key(), minecraft),
            None => format!("vanilla|{minecraft}"),
        },
        loader_label: loader
            .map(|id| id.display_name().to_string())
            .unwrap_or_else(|| "Vanilla".to_string()),
        loader: loader.map(|id| id.short_key().to_string()),
        minecraft,
    })
}

pub async fn modpack_install(
    state: &AppState,
    request: ModpackInstallRequest,
) -> Result<ModpackInstallResponse, ContentApiError> {
    if state
        .sessions()
        .has_active_instance(&request.instance_id)
        .await
    {
        return Err(json_error(
            StatusCode::CONFLICT,
            "cannot install a modpack while the instance is running; stop the game first",
        ));
    }
    let target = instance_target(state, &request.instance_id).await?;
    let game_dir = target
        .game_dir
        .clone()
        .ok_or_else(|| json_error(StatusCode::NOT_FOUND, "instance not found"))?;

    let id = CanonicalId(request.canonical_id.clone());
    let detail = state
        .content()
        .detail(&id)
        .await
        .map_err(content_error_response)?;
    if detail.content.kind != ContentKind::Modpack {
        return Err(json_error(StatusCode::BAD_REQUEST, "this is not a modpack"));
    }
    let version =
        pick_version(&detail.versions, request.version_id.as_deref()).ok_or_else(|| {
            json_error(
                StatusCode::NOT_FOUND,
                "this modpack has no installable version",
            )
        })?;
    let archive_file = version.primary_file().cloned().ok_or_else(|| {
        json_error(
            StatusCode::NOT_FOUND,
            "this modpack version has no downloadable file",
        )
    })?;

    let pack_title = detail.content.title.clone();
    let archive = download_archive(state, &game_dir, &archive_file).await?;
    let install = install_pack(state.content().client(), &game_dir, &archive, |_| {}).await;
    let _ = std::fs::remove_file(&archive);
    let report = install.map_err(content_error_response)?;

    let identified = record_pack_provenance(
        state,
        &game_dir,
        &report.installed,
        &id,
        &pack_title,
        version,
    )
    .await?;

    let mismatch = mismatch_notice(
        &target.loader,
        &target.game_version,
        report
            .index
            .loader
            .as_ref()
            .map(|loader| loader.key.as_str()),
        &report.index.minecraft,
    );

    Ok(ModpackInstallResponse {
        instance_id: request.instance_id,
        name: report.index.name,
        version: report.index.version,
        minecraft: report.index.minecraft,
        loader: report.index.loader.map(|loader| loader.key),
        file_count: report.installed.len(),
        overrides_applied: report.overrides_applied,
        identified_count: identified,
        mismatch,
    })
}

/// Pull the `.mrpack` itself into a scratch file next to the instance, verified
/// like any other download.
async fn download_archive(
    state: &AppState,
    game_dir: &Path,
    file: &FileRef,
) -> Result<PathBuf, ContentApiError> {
    let archive = game_dir.join(format!(".axial-pack-{}", sanitize(&file.filename)));
    if let Some(parent) = archive.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| content_error_response(axial_content::ContentError::Io(error)))?;
    }
    let expected = axial_minecraft::download::ExpectedIntegrity {
        size: file.size,
        sha1: file.sha1.clone(),
    };
    axial_minecraft::download::download_file_with_client_report(
        state.content().client(),
        &file.url,
        &archive,
        &expected,
    )
    .await
    .map_err(|error| {
        content_error_response(axial_content::ContentError::Download(
            error.into_download_error().to_string(),
        ))
    })?;
    Ok(archive)
}

/// Ask the provider to name every file the pack just laid down, using the
/// hashes the index already gave us, and record what it recognizes. Files it
/// does not recognize stay unmanaged rather than being invented.
async fn record_pack_provenance(
    state: &AppState,
    game_dir: &Path,
    installed: &[axial_content::PackFile],
    pack_id: &CanonicalId,
    pack_title: &str,
    version: &axial_content::ContentVersion,
) -> Result<usize, ContentApiError> {
    let mut manifest = ContentManifest::load(game_dir).map_err(content_error_response)?;

    // The pack itself: what this instance was built from, so an update knows
    // where it came from.
    manifest.upsert(ManifestEntry {
        canonical_id: pack_id.clone(),
        provider: ProviderId::Modrinth,
        project_id: pack_id.project_id().to_string(),
        version_id: version.id.clone(),
        kind: ContentKind::Modpack,
        filename: String::new(),
        sha1: None,
        sha512: None,
        size: None,
        dependencies: Vec::new(),
        enabled: true,
        source: EntrySource::Managed,
        installed_at: chrono::Utc::now().to_rfc3339(),
        title: Some(pack_title.to_string()),
    });

    let by_hash: HashMap<String, &axial_content::PackFile> = installed
        .iter()
        .filter(|file| file.kind().is_some())
        .filter_map(|file| file.sha512.clone().map(|hash| (hash, file)))
        .collect();

    let mut identified = 0;
    if !by_hash.is_empty() {
        let hashes: Vec<String> = by_hash.keys().cloned().collect();
        if let Ok(resolved) = state.content().identify(&hashes).await {
            let ids: Vec<CanonicalId> = resolved
                .values()
                .map(|identity| CanonicalId::for_project(identity.provider, &identity.project_id))
                .collect();
            let titles = super::project_titles(state, &ids).await;

            for (hash, identity) in resolved {
                let Some(file) = by_hash.get(&hash) else {
                    continue;
                };
                let Some(kind) = file.kind() else { continue };
                let canonical_id =
                    CanonicalId::for_project(identity.provider, &identity.project_id);
                let title = titles.get(&canonical_id).cloned().or(identity.title);
                manifest.upsert(ManifestEntry {
                    canonical_id,
                    provider: identity.provider,
                    project_id: identity.project_id,
                    version_id: identity.version_id,
                    kind,
                    filename: file.filename().to_string(),
                    sha1: file.sha1.clone(),
                    sha512: file.sha512.clone(),
                    size: file.size,
                    dependencies: Vec::new(),
                    enabled: true,
                    source: EntrySource::Managed,
                    installed_at: chrono::Utc::now().to_rfc3339(),
                    title,
                });
                identified += 1;
            }
        }
    }

    manifest.save(game_dir).map_err(content_error_response)?;
    Ok(identified)
}

fn mismatch_notice(
    instance_loader: &str,
    instance_minecraft: &str,
    pack_loader: Option<&str>,
    pack_minecraft: &str,
) -> Option<String> {
    let instance_loader = if instance_loader.is_empty() {
        "vanilla"
    } else {
        instance_loader
    };
    let pack_loader = pack_loader.unwrap_or("vanilla");
    if instance_loader == pack_loader && instance_minecraft == pack_minecraft {
        return None;
    }
    Some(format!(
        "this pack targets {pack_loader} {pack_minecraft}, but the instance is {instance_loader} {instance_minecraft}"
    ))
}

fn is_release_version(value: &str) -> bool {
    !value.is_empty()
        && value
            .split('.')
            .all(|part| !part.is_empty() && part.chars().all(|c| c.is_ascii_digit()))
}

fn sanitize(filename: &str) -> String {
    filename
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_matching_instance_raises_no_mismatch() {
        assert_eq!(
            mismatch_notice("fabric", "1.21.6", Some("fabric"), "1.21.6"),
            None
        );
        assert_eq!(mismatch_notice("", "1.21.6", None, "1.21.6"), None);
    }

    #[test]
    fn a_mismatch_names_both_sides() {
        let notice =
            mismatch_notice("fabric", "1.21.4", Some("fabric"), "1.21.6").expect("versions differ");
        assert!(notice.contains("1.21.6"));
        assert!(notice.contains("1.21.4"));

        let notice =
            mismatch_notice("forge", "1.21.6", Some("fabric"), "1.21.6").expect("loaders differ");
        assert!(notice.contains("fabric"));
        assert!(notice.contains("forge"));
    }

    #[test]
    fn the_scratch_archive_name_cannot_escape_the_instance() {
        assert_eq!(sanitize("../../evil.mrpack"), "------evil-mrpack");
        assert_eq!(sanitize("Cobblemon.mrpack"), "Cobblemon-mrpack");
    }
}
