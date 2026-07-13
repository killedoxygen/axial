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
use crate::application::{
    InstallQueueContentActionRequest, InstallQueueRequest, InstallQueueStateResponse,
    enqueue_install_with_dependency,
};
use crate::state::AppState;
use axial_content::{
    CanonicalId, ContentKind, ContentManifest, EntrySource, FileRef, ManifestEntry, ProviderId,
    install_pack_files_with_finalize, read_pack_index,
};
use axum::http::StatusCode;
use futures_util::{StreamExt, stream};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static PACK_ARCHIVE_SEQUENCE: AtomicU64 = AtomicU64::new(1);

struct ScratchArchive {
    path: PathBuf,
}

impl ScratchArchive {
    fn new(path: PathBuf) -> Self {
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for ScratchArchive {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[derive(Debug, Deserialize)]
pub struct ModpackInstallRequest {
    pub instance_id: String,
    pub canonical_id: String,
    #[serde(default)]
    pub version_id: Option<String>,
    #[serde(default)]
    pub selected_paths: Vec<String>,
    #[serde(default = "default_true")]
    pub include_overrides: bool,
}

fn default_true() -> bool {
    true
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

#[derive(Debug, Serialize)]
pub struct ModpackFileOption {
    pub path: String,
    pub filename: String,
    pub kind: ContentKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    pub title: String,
    pub identified: bool,
    pub compatible: bool,
    pub installed: bool,
}

#[derive(Debug, Serialize)]
pub struct ModpackFilesPlan {
    pub canonical_id: CanonicalId,
    pub version_id: String,
    pub name: String,
    pub minecraft: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub loader: Option<String>,
    pub files: Vec<ModpackFileOption>,
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

pub async fn modpack_files(
    state: &AppState,
    instance_id: &str,
    canonical_id: &str,
    version_id: Option<&str>,
) -> Result<ModpackFilesPlan, ContentApiError> {
    let target = instance_target(state, instance_id).await?;
    let game_dir = target
        .game_dir
        .clone()
        .ok_or_else(|| json_error(StatusCode::NOT_FOUND, "instance not found"))?;
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
    let archive_file = version.primary_file().cloned().ok_or_else(|| {
        json_error(
            StatusCode::NOT_FOUND,
            "this modpack version has no downloadable file",
        )
    })?;
    let archive = download_archive(state, &game_dir, &archive_file).await?;
    let index = read_pack_index(archive.path()).map_err(content_error_response)?;

    let hashes: Vec<String> = index
        .files
        .iter()
        .filter_map(|file| file.sha512.clone())
        .collect();
    let identities = if hashes.is_empty() {
        HashMap::new()
    } else {
        state.content().identify(&hashes).await.unwrap_or_default()
    };
    let identity_versions: HashMap<(String, String), Option<(bool, bool, String)>> = stream::iter(
        identities
            .values()
            .map(|identity| {
                (
                    CanonicalId::for_project(identity.provider, &identity.project_id),
                    identity.version_id.clone(),
                )
            })
            .collect::<std::collections::HashSet<_>>(),
    )
    .map(|(project_id, version_id)| {
        let state = state.clone();
        let game_version = target.game_version.clone();
        let loader = target.loader.clone();
        async move {
            let key = (project_id.as_str().to_string(), version_id.clone());
            let compatible = state
                .content()
                .detail(&project_id)
                .await
                .ok()
                .and_then(|detail| {
                    let title = detail.content.title;
                    detail
                        .versions
                        .into_iter()
                        .find(|version| version.id == version_id)
                        .map(|version| {
                            let game = version
                                .game_versions
                                .iter()
                                .any(|game| game == &game_version);
                            let loader =
                                version.loaders.iter().any(|candidate| candidate == &loader);
                            (game, loader, title)
                        })
                });
            (key, compatible)
        }
    })
    .buffer_unordered(8)
    .collect()
    .await;
    let mut files = Vec::new();
    for file in &index.files {
        let Some(kind) = file.kind() else { continue };
        let identity = file.sha512.as_ref().and_then(|hash| identities.get(hash));
        let compatible = if let Some(identity) = identity {
            let project_id = CanonicalId::for_project(identity.provider, &identity.project_id);
            identity_versions
                .get(&(project_id.as_str().to_string(), identity.version_id.clone()))
                .and_then(Option::as_ref)
                .is_some_and(|(game, loader, _)| *game && (kind != ContentKind::Mod || *loader))
        } else {
            false
        };
        files.push(ModpackFileOption {
            path: file.path.clone(),
            filename: file.filename().to_string(),
            kind,
            size: file.size,
            title: identity
                .and_then(|identity| {
                    let project_id =
                        CanonicalId::for_project(identity.provider, &identity.project_id);
                    identity_versions
                        .get(&(project_id.as_str().to_string(), identity.version_id.clone()))
                        .and_then(|compatibility| compatibility.as_ref())
                        .map(|(_, _, title)| title.clone())
                        .or_else(|| identity.title.clone())
                })
                .unwrap_or_else(|| file.filename().to_string()),
            identified: identity.is_some(),
            compatible,
            installed: game_dir.join(&file.path).exists(),
        });
    }
    files.sort_by(|left, right| left.title.to_lowercase().cmp(&right.title.to_lowercase()));

    Ok(ModpackFilesPlan {
        canonical_id: id,
        version_id: version.id.clone(),
        name: detail.content.title,
        minecraft: index.minecraft,
        loader: index.loader.map(|loader| loader.key),
        files,
    })
}

pub async fn queue_modpack_install(
    state: &AppState,
    request: ModpackInstallRequest,
) -> Result<InstallQueueStateResponse, ContentApiError> {
    queue_modpack_install_after(state, request, None, false).await
}

pub(crate) async fn queue_modpack_install_after(
    state: &AppState,
    request: ModpackInstallRequest,
    prerequisite_queue_id: Option<String>,
    remove_instance_on_failure: bool,
) -> Result<InstallQueueStateResponse, ContentApiError> {
    let target =
        modpack_target(state, &request.canonical_id, request.version_id.as_deref()).await?;
    enqueue_install_with_dependency(
        state,
        pinned_modpack_queue_request(request, target),
        prerequisite_queue_id,
        remove_instance_on_failure,
    )
    .await
}

fn pinned_modpack_queue_request(
    request: ModpackInstallRequest,
    target: ModpackTarget,
) -> InstallQueueRequest {
    let label = if request.selected_paths.is_empty() {
        format!("Setting up {}", target.name)
    } else {
        format!(
            "Adding {} files from {}",
            request.selected_paths.len(),
            target.name
        )
    };
    InstallQueueRequest {
        kind: "content".to_string(),
        instance_id: request.instance_id,
        label,
        content_action: Some(InstallQueueContentActionRequest::Modpack {
            canonical_id: target.canonical_id.as_str().to_string(),
            version_id: target.version_id,
            selected_paths: request.selected_paths,
            include_overrides: request.include_overrides,
        }),
        ..InstallQueueRequest::default()
    }
}

pub(crate) async fn execute_modpack_install<F>(
    state: &AppState,
    request: ModpackInstallRequest,
    mut on_progress: F,
) -> Result<ModpackInstallResponse, ContentApiError>
where
    F: FnMut(axial_minecraft::DownloadProgress),
{
    on_progress(axial_minecraft::DownloadProgress {
        phase: "planning".to_string(),
        current: 0,
        total: 1,
        file: None,
        error: None,
        done: false,
        bytes_done: None,
        bytes_total: None,
    });
    let _lifecycle_guard = super::lock_instance_for_content_mutation(state, &request.instance_id)?;
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
    let preview = read_pack_index(archive.path()).map_err(content_error_response)?;
    let preview_files: Vec<axial_content::PackFile> = preview
        .files
        .iter()
        .filter(|file| {
            request.selected_paths.is_empty()
                || request.selected_paths.iter().any(|path| path == &file.path)
        })
        .cloned()
        .collect();
    let (manifest, identified) = build_pack_manifest(
        state,
        &game_dir,
        &preview_files,
        &id,
        &pack_title,
        version,
        request.selected_paths.is_empty(),
    )
    .await?;
    let install = install_pack_files_with_finalize(
        state.content().client(),
        &game_dir,
        archive.path(),
        &request.selected_paths,
        request.include_overrides,
        &mut on_progress,
        |_| manifest.save(&game_dir),
    )
    .await;
    let report = install.map_err(content_error_response)?;

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
) -> Result<ScratchArchive, ContentApiError> {
    let sequence = PACK_ARCHIVE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let archive = ScratchArchive::new(game_dir.join(format!(
        ".axial-pack-{sequence:x}-{}",
        sanitize(&file.filename)
    )));
    if let Some(parent) = archive.path().parent() {
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
        archive.path(),
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
async fn build_pack_manifest(
    state: &AppState,
    game_dir: &Path,
    installed: &[axial_content::PackFile],
    pack_id: &CanonicalId,
    pack_title: &str,
    version: &axial_content::ContentVersion,
    record_pack_root: bool,
) -> Result<(ContentManifest, usize), ContentApiError> {
    let mut manifest = ContentManifest::load(game_dir).map_err(content_error_response)?;

    // The pack itself: what this instance was built from, so an update knows
    // where it came from.
    if record_pack_root {
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
    }

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

    Ok((manifest, identified))
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

    #[test]
    fn scratch_archive_is_removed_when_its_guard_drops() {
        let path = std::env::temp_dir().join(format!(
            "axial-scratch-archive-test-{}-{}",
            std::process::id(),
            PACK_ARCHIVE_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::write(&path, b"scratch").expect("write scratch archive");

        {
            let archive = ScratchArchive::new(path.clone());
            assert_eq!(archive.path(), path.as_path());
            assert!(path.exists());
        }

        assert!(!path.exists());
    }

    #[test]
    fn omitted_modpack_version_is_replaced_with_the_resolved_version() {
        let request = pinned_modpack_queue_request(
            ModpackInstallRequest {
                instance_id: "instance-1".to_string(),
                canonical_id: "modrinth:pack".to_string(),
                version_id: None,
                selected_paths: Vec::new(),
                include_overrides: true,
            },
            ModpackTarget {
                canonical_id: CanonicalId("modrinth:pack".to_string()),
                version_id: "resolved-version".to_string(),
                name: "Pack".to_string(),
                minecraft: "1.21.1".to_string(),
                loader: Some("fabric".to_string()),
                loader_label: "Fabric".to_string(),
                selection_id: "loader_version|fabric|1.21.1".to_string(),
            },
        );

        let Some(InstallQueueContentActionRequest::Modpack { version_id, .. }) =
            request.content_action
        else {
            panic!("expected queued modpack action");
        };
        assert_eq!(version_id, "resolved-version");
    }
}
