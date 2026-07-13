//! Installing a modpack into an instance. The frontend creates the instance
//! first — the pack's loader and Minecraft version are enough to address the
//! create API — and then hands the empty instance here to be filled.
//!
//! Pack files carry sha512 hashes in the index, so once they are on disk we can
//! ask the provider what they are in one batch and record real provenance for
//! every mod, rather than leaving a pack-shaped hole in the manifest.

use super::resolve::{pick_version, resolve};
use super::target::{ResolveTarget, instance_target};
use super::{ContentApiError, ContentSelection, content_error_response, json_error};
use crate::application::{
    InstallQueueContentActionRequest, InstallQueueRequest, InstallQueueStateResponse,
    enqueue_install_with_dependency,
};
use crate::state::AppState;
use axial_content::{
    CanonicalId, ContentKind, ContentManifest, EntrySource, FileRef, ManifestEntry, PackIndex,
    ProviderId, VersionIdentity, install_pack_files_with_finalize, read_pack_index,
    verified_removable_variants,
};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

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

struct ResolvedModpackVersion {
    canonical_id: CanonicalId,
    name: String,
    version: axial_content::ContentVersion,
}

#[derive(Debug)]
struct PackFileCompatibility {
    game: bool,
    loader: bool,
    title: String,
}

type PackCompatibilityMap = HashMap<(String, String), PackFileCompatibility>;

async fn resolve_modpack_version(
    state: &AppState,
    canonical_id: &str,
    version_id: Option<&str>,
) -> Result<ResolvedModpackVersion, ContentApiError> {
    let canonical_id = CanonicalId(canonical_id.to_string());
    let detail = state
        .content()
        .detail(&canonical_id)
        .await
        .map_err(content_error_response)?;
    if detail.content.kind != ContentKind::Modpack {
        return Err(json_error(StatusCode::BAD_REQUEST, "this is not a modpack"));
    }
    let version = pick_version(&detail.versions, version_id)
        .cloned()
        .ok_or_else(|| {
            json_error(
                StatusCode::NOT_FOUND,
                "this modpack has no installable version",
            )
        })?;
    Ok(ResolvedModpackVersion {
        canonical_id,
        name: detail.content.title,
        version,
    })
}

/// Read the pack-authored loader and Minecraft requirements and pin creation to
/// that exact loader build. Provider summary metadata only names the loader
/// family and cannot safely substitute for `modrinth.index.json`.
pub async fn modpack_target(
    state: &AppState,
    canonical_id: &str,
    version_id: Option<&str>,
) -> Result<ModpackTarget, ContentApiError> {
    let resolved = resolve_modpack_version(state, canonical_id, version_id).await?;
    let archive_file = resolved.version.primary_file().cloned().ok_or_else(|| {
        json_error(
            StatusCode::NOT_FOUND,
            "this modpack version has no downloadable file",
        )
    })?;
    let archive = download_archive(state, &archive_file).await?;
    let index = read_pack_index(archive.path()).map_err(content_error_response)?;

    target_from_pack_index(
        resolved.canonical_id,
        resolved.version.id,
        resolved.name,
        &index,
    )
}

fn target_from_pack_index(
    canonical_id: CanonicalId,
    version_id: String,
    name: String,
    index: &PackIndex,
) -> Result<ModpackTarget, ContentApiError> {
    let (loader, loader_label, selection_id) = match index.loader.as_ref() {
        Some(loader) => {
            let component =
                axial_minecraft::LoaderComponentId::parse(&loader.key).ok_or_else(|| {
                    json_error(
                        StatusCode::BAD_REQUEST,
                        "this modpack uses an unsupported loader",
                    )
                })?;
            let build_id =
                axial_minecraft::build_id_for(component, &index.minecraft, &loader.version);
            (
                Some(component.short_key().to_string()),
                component.display_name().to_string(),
                format!("loader_build|{}|{build_id}", component.as_str()),
            )
        }
        None => (
            None,
            "Vanilla".to_string(),
            format!("vanilla|{}", index.minecraft),
        ),
    };

    Ok(ModpackTarget {
        canonical_id,
        version_id,
        name,
        minecraft: index.minecraft.clone(),
        loader,
        loader_label,
        selection_id,
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
    let resolved = resolve_modpack_version(state, canonical_id, version_id).await?;
    let archive_file = resolved.version.primary_file().cloned().ok_or_else(|| {
        json_error(
            StatusCode::NOT_FOUND,
            "this modpack version has no downloadable file",
        )
    })?;
    let archive = download_archive(state, &archive_file).await?;
    let index = read_pack_index(archive.path()).map_err(content_error_response)?;
    let identities = identify_modpack_files(state, &index).await?;
    let files = classify_modpack_files(state, &target, &game_dir, &index, &identities).await;

    Ok(ModpackFilesPlan {
        canonical_id: resolved.canonical_id,
        version_id: resolved.version.id,
        name: resolved.name,
        minecraft: index.minecraft,
        loader: index.loader.map(|loader| loader.key),
        files,
    })
}

async fn classify_modpack_files(
    state: &AppState,
    target: &ResolveTarget,
    game_dir: &Path,
    index: &PackIndex,
    identities: &HashMap<String, VersionIdentity>,
) -> Vec<ModpackFileOption> {
    let project_ids: Vec<CanonicalId> = identities
        .values()
        .map(|identity| CanonicalId::for_project(identity.provider, &identity.project_id))
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    let metadata = state
        .content()
        .metadata(&project_ids)
        .await
        .unwrap_or_default();
    let identity_versions: PackCompatibilityMap = identities
        .values()
        .map(|identity| {
            let project_id = CanonicalId::for_project(identity.provider, &identity.project_id);
            let game = identity
                .game_versions
                .iter()
                .any(|game| game == &target.game_version);
            let loader = identity
                .loaders
                .iter()
                .any(|candidate| candidate == &target.loader);
            let title = metadata
                .get(&project_id)
                .map(|project| project.title.clone())
                .or_else(|| identity.title.clone())
                .unwrap_or_else(|| identity.project_id.clone());
            (
                (project_id.as_str().to_string(), identity.version_id.clone()),
                PackFileCompatibility {
                    game,
                    loader,
                    title,
                },
            )
        })
        .collect();

    classify_modpack_file_options(game_dir, index, identities, &identity_versions)
}

async fn identify_modpack_files(
    state: &AppState,
    index: &PackIndex,
) -> Result<HashMap<String, VersionIdentity>, ContentApiError> {
    let hashes: Vec<String> = index
        .files
        .iter()
        .filter_map(|file| file.sha512.clone())
        .collect();
    if hashes.is_empty() {
        Ok(HashMap::new())
    } else {
        state
            .content()
            .identify(&hashes)
            .await
            .map_err(content_error_response)
    }
}

fn classify_modpack_file_options(
    game_dir: &Path,
    index: &PackIndex,
    identities: &HashMap<String, VersionIdentity>,
    identity_versions: &PackCompatibilityMap,
) -> Vec<ModpackFileOption> {
    let mut files = Vec::new();
    for file in &index.files {
        let Some(kind) = file.kind() else { continue };
        let identity = file.sha512.as_ref().and_then(|hash| identities.get(hash));
        let compatible = if let Some(identity) = identity {
            let project_id = CanonicalId::for_project(identity.provider, &identity.project_id);
            identity_versions
                .get(&(project_id.as_str().to_string(), identity.version_id.clone()))
                .is_some_and(|compatibility| {
                    compatibility.game && (kind != ContentKind::Mod || compatibility.loader)
                })
        } else {
            false
        };
        let installed = [
            game_dir.join(&file.path),
            game_dir.join(format!("{}.disabled", file.path)),
        ]
        .into_iter()
        .any(|path| std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_file()));
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
                        .map(|compatibility| compatibility.title.clone())
                        .or_else(|| identity.title.clone())
                })
                .unwrap_or_else(|| file.filename().to_string()),
            identified: identity.is_some(),
            compatible,
            installed,
        });
    }
    files.sort_by_key(|file| file.title.to_lowercase());
    files
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
    reject_cherry_pick_overrides(&request)?;
    let resolved =
        resolve_modpack_version(state, &request.canonical_id, request.version_id.as_deref())
            .await?;
    enqueue_install_with_dependency(
        state,
        pinned_modpack_queue_request(
            request,
            resolved.canonical_id,
            resolved.version.id,
            resolved.name,
        ),
        prerequisite_queue_id,
        remove_instance_on_failure,
    )
    .await
}

fn pinned_modpack_queue_request(
    request: ModpackInstallRequest,
    canonical_id: CanonicalId,
    version_id: String,
    name: String,
) -> InstallQueueRequest {
    let include_overrides = request.selected_paths.is_empty() && request.include_overrides;
    let label = if request.selected_paths.is_empty() {
        format!("Setting up {name}")
    } else {
        format!(
            "Adding {} files from {}",
            request.selected_paths.len(),
            name
        )
    };
    InstallQueueRequest {
        kind: "content".to_string(),
        instance_id: request.instance_id,
        label,
        content_action: Some(InstallQueueContentActionRequest::Modpack {
            canonical_id: canonical_id.as_str().to_string(),
            version_id,
            selected_paths: request.selected_paths,
            include_overrides,
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
    reject_cherry_pick_overrides(&request)?;
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

    let resolved =
        resolve_modpack_version(state, &request.canonical_id, request.version_id.as_deref())
            .await?;
    let archive_file = resolved.version.primary_file().cloned().ok_or_else(|| {
        json_error(
            StatusCode::NOT_FOUND,
            "this modpack version has no downloadable file",
        )
    })?;

    let archive = download_archive(state, &archive_file).await?;
    let preview = read_pack_index(archive.path()).map_err(content_error_response)?;
    if !request.selected_paths.is_empty() {
        let identities = identify_modpack_files(state, &preview).await?;
        let classified =
            classify_modpack_files(state, &target, &game_dir, &preview, &identities).await;
        validate_cherry_pick_files(&request.selected_paths, &classified)?;
        validate_cherry_pick_dependencies(
            state,
            &target,
            &game_dir,
            &preview,
            &request.selected_paths,
            &identities,
        )
        .await?;
    }
    let preview_files: Vec<axial_content::PackFile> = preview
        .files
        .iter()
        .filter(|file| {
            request.selected_paths.is_empty()
                || request.selected_paths.iter().any(|path| path == &file.path)
        })
        .cloned()
        .collect();
    let (manifest, identified, stale_entries) = build_pack_manifest(
        state,
        &game_dir,
        &preview_files,
        &resolved.canonical_id,
        &resolved.name,
        &resolved.version,
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
        |report, transaction| {
            let protected_paths: Vec<String> = report
                .installed
                .iter()
                .map(|file| file.path.clone())
                .collect();
            let stale_files =
                verified_stale_pack_files(&game_dir, &stale_entries, &protected_paths)?;
            transaction.stage_removals(&stale_files)?;
            manifest.save(&game_dir)
        },
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

fn reject_cherry_pick_overrides(request: &ModpackInstallRequest) -> Result<(), ContentApiError> {
    if !request.selected_paths.is_empty() && request.include_overrides {
        return Err(json_error(
            StatusCode::BAD_REQUEST,
            "modpack overrides cannot be applied with selected files",
        ));
    }
    Ok(())
}

fn validate_cherry_pick_files(
    selected_paths: &[String],
    classified: &[ModpackFileOption],
) -> Result<(), ContentApiError> {
    let allowed: HashSet<&str> = classified
        .iter()
        .filter(|file| file.identified && file.compatible && !file.installed)
        .map(|file| file.path.as_str())
        .collect();
    let selected: HashSet<&str> = selected_paths.iter().map(String::as_str).collect();
    if selected.len() != selected_paths.len() || selected.iter().any(|path| !allowed.contains(path))
    {
        return Err(json_error(
            StatusCode::CONFLICT,
            "selected modpack files are no longer compatible or available; review them and try again",
        ));
    }
    Ok(())
}

async fn validate_cherry_pick_dependencies(
    state: &AppState,
    target: &ResolveTarget,
    game_dir: &Path,
    index: &PackIndex,
    selected_paths: &[String],
    identities: &HashMap<String, VersionIdentity>,
) -> Result<(), ContentApiError> {
    let files: HashMap<&str, &axial_content::PackFile> = index
        .files
        .iter()
        .map(|file| (file.path.as_str(), file))
        .collect();
    let mut selections = Vec::with_capacity(selected_paths.len());
    let mut selected_versions = HashSet::with_capacity(selected_paths.len());
    for path in selected_paths {
        let Some(file) = files.get(path.as_str()) else {
            return Err(cherry_pick_conflict());
        };
        let Some(kind) = file.kind() else {
            return Err(cherry_pick_conflict());
        };
        let Some(identity) = file.sha512.as_ref().and_then(|hash| identities.get(hash)) else {
            return Err(cherry_pick_conflict());
        };
        let canonical_id = CanonicalId::for_project(identity.provider, &identity.project_id);
        if !selected_versions.insert((canonical_id.clone(), identity.version_id.clone())) {
            return Err(cherry_pick_conflict());
        }
        selections.push(ContentSelection {
            canonical_id: canonical_id.as_str().to_string(),
            kind,
            version_id: Some(identity.version_id.clone()),
        });
    }

    let manifest = ContentManifest::load(game_dir).map_err(content_error_response)?;
    let resolution = resolve(state, target, &selections, &manifest).await?;
    if !cherry_pick_resolution_is_complete(&resolution, &selected_versions) {
        return Err(cherry_pick_conflict());
    }
    Ok(())
}

fn cherry_pick_resolution_is_complete(
    resolution: &super::resolve::Resolution,
    selected_versions: &HashSet<(CanonicalId, String)>,
) -> bool {
    resolution.conflicts.is_empty()
        && resolution.items.iter().all(|item| {
            (item.already_installed && !item.update)
                || selected_versions.contains(&(item.canonical_id.clone(), item.version_id.clone()))
        })
}

fn cherry_pick_conflict() -> ContentApiError {
    json_error(
        StatusCode::CONFLICT,
        "selected modpack files require other files or conflict with installed content; review the selection and try again",
    )
}

/// Pull the `.mrpack` into a process-unique temporary file, verified like any
/// other download. The guard removes it on every return path.
async fn download_archive(
    state: &AppState,
    file: &FileRef,
) -> Result<ScratchArchive, ContentApiError> {
    let archive = ScratchArchive::new(std::env::temp_dir().join(format!(
        ".axial-pack-{}-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4(),
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
) -> Result<(ContentManifest, usize, Vec<ManifestEntry>), ContentApiError> {
    let mut manifest = ContentManifest::load(game_dir).map_err(content_error_response)?;
    let mut stale_entries = Vec::new();

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

    let by_hash = group_pack_files_by_sha512(installed);
    reject_duplicate_pack_hashes(&by_hash)?;

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
                let Some(file) = by_hash.get(&hash).and_then(|files| files.first()) else {
                    continue;
                };
                let Some(kind) = file.kind() else { continue };
                let canonical_id =
                    CanonicalId::for_project(identity.provider, &identity.project_id);
                let previous = manifest.find(&canonical_id).cloned();
                let title = titles
                    .get(&canonical_id)
                    .cloned()
                    .or(identity.title.clone());
                let stale_filename = manifest.upsert(ManifestEntry {
                    canonical_id,
                    provider: identity.provider,
                    project_id: identity.project_id,
                    version_id: identity.version_id,
                    kind,
                    filename: file.filename().to_string(),
                    sha1: file.sha1.clone(),
                    sha512: file.sha512.clone(),
                    size: file.size,
                    dependencies: identity.dependencies,
                    enabled: true,
                    source: EntrySource::Managed,
                    installed_at: chrono::Utc::now().to_rfc3339(),
                    title,
                });
                if stale_filename.is_some()
                    && let Some(previous) = previous
                {
                    stale_entries.push(previous);
                }
                identified += 1;
            }
        }
    }

    Ok((manifest, identified, stale_entries))
}

fn group_pack_files_by_sha512(
    installed: &[axial_content::PackFile],
) -> HashMap<String, Vec<&axial_content::PackFile>> {
    let mut grouped: HashMap<String, Vec<&axial_content::PackFile>> = HashMap::new();
    for file in installed.iter().filter(|file| file.kind().is_some()) {
        if let Some(hash) = &file.sha512 {
            grouped.entry(hash.clone()).or_default().push(file);
        }
    }
    grouped
}

fn reject_duplicate_pack_hashes(
    grouped: &HashMap<String, Vec<&axial_content::PackFile>>,
) -> Result<(), ContentApiError> {
    if grouped.values().any(|files| files.len() > 1) {
        return Err(json_error(
            StatusCode::BAD_REQUEST,
            "modpack repeats the same managed content at multiple paths",
        ));
    }
    Ok(())
}

fn verified_stale_pack_files(
    game_dir: &Path,
    stale_entries: &[ManifestEntry],
    protected_paths: &[String],
) -> axial_content::ContentResult<Vec<String>> {
    let mut files = Vec::new();
    for entry in stale_entries {
        files.extend(verified_removable_variants(
            game_dir,
            entry,
            protected_paths,
        )?);
    }
    files.sort();
    files.dedup();
    Ok(files)
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
    fn duplicate_pack_hashes_are_rejected_before_manifest_finalization() {
        let file = |path: &str, hash: &str| axial_content::PackFile {
            path: path.to_string(),
            url: format!("https://example.invalid/{path}"),
            sha1: None,
            sha512: Some(hash.to_string()),
            size: Some(42),
        };
        let installed = vec![
            file("mods/first.jar", "shared-hash"),
            file("mods/second.jar", "shared-hash"),
        ];

        let grouped = group_pack_files_by_sha512(&installed);
        assert_eq!(grouped["shared-hash"].len(), 2);
        let (status, _) = reject_duplicate_pack_hashes(&grouped)
            .expect_err("one manifest entry cannot safely own two paths");
        assert_eq!(status, StatusCode::BAD_REQUEST);
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
            uuid::Uuid::new_v4()
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
    fn stale_pack_cleanup_preserves_a_manual_replacement() {
        let root = std::env::temp_dir().join(format!(
            "axial-pack-stale-replacement-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(root.join("mods")).expect("mods");
        let path = root.join("mods/old.jar");
        std::fs::write(&path, b"tracked bytes").expect("tracked file");
        let mut entry = ManifestEntry::managed(
            CanonicalId::for_project(ProviderId::Modrinth, "project"),
            ProviderId::Modrinth,
            "project".to_string(),
            "old-version".to_string(),
            ContentKind::Mod,
            &FileRef {
                url: "https://example.invalid/old.jar".to_string(),
                filename: "old.jar".to_string(),
                sha1: None,
                sha512: None,
                size: Some(b"tracked bytes".len() as u64),
                primary: true,
            },
            Vec::new(),
            None,
        );
        entry.sha512 = Some(axial_content::sha512_file(&path).expect("tracked hash"));

        std::fs::write(&path, b"user replacement").expect("replace tracked file");
        assert!(verified_stale_pack_files(&root, &[entry], &[]).is_err());
        assert_eq!(
            std::fs::read(&path).expect("preserved replacement"),
            b"user replacement"
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn modpack_target_pins_the_loader_version_from_the_pack_index() {
        let target = target_from_pack_index(
            CanonicalId("modrinth:pack".to_string()),
            "pack-version".to_string(),
            "Pack".to_string(),
            &PackIndex {
                name: "Pack".to_string(),
                version: "1.0.0".to_string(),
                minecraft: "1.20.1".to_string(),
                loader: Some(axial_content::PackLoader {
                    key: "fabric".to_string(),
                    version: "0.14.22".to_string(),
                }),
                files: Vec::new(),
            },
        )
        .expect("pack target");

        assert_eq!(target.minecraft, "1.20.1");
        assert_eq!(target.loader.as_deref(), Some("fabric"));
        assert_eq!(
            target.selection_id,
            "loader_build|net.fabricmc.fabric-loader|fabric:1.20.1:0.14.22"
        );
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
            CanonicalId("modrinth:pack".to_string()),
            "resolved-version".to_string(),
            "Pack".to_string(),
        );

        let Some(InstallQueueContentActionRequest::Modpack { version_id, .. }) =
            request.content_action
        else {
            panic!("expected queued modpack action");
        };
        assert_eq!(version_id, "resolved-version");
    }

    #[test]
    fn selected_files_cannot_enable_pack_overrides() {
        let request = ModpackInstallRequest {
            instance_id: "instance-1".to_string(),
            canonical_id: "modrinth:pack".to_string(),
            version_id: Some("version".to_string()),
            selected_paths: vec!["mods/example.jar".to_string()],
            include_overrides: true,
        };

        assert!(reject_cherry_pick_overrides(&request).is_err());
    }

    #[test]
    fn cherry_pick_validation_rejects_unidentified_incompatible_and_occupied_files() {
        let option =
            |path: &str, identified: bool, compatible: bool, installed: bool| ModpackFileOption {
                path: path.to_string(),
                filename: path.rsplit('/').next().unwrap_or(path).to_string(),
                kind: ContentKind::Mod,
                size: None,
                title: "Example".to_string(),
                identified,
                compatible,
                installed,
            };
        let classified = vec![
            option("mods/good.jar", true, true, false),
            option("mods/unknown.jar", false, false, false),
            option("mods/incompatible.jar", true, false, false),
            option("mods/occupied.jar", true, true, true),
        ];

        assert!(validate_cherry_pick_files(&["mods/good.jar".to_string()], &classified).is_ok());
        for rejected in [
            "mods/unknown.jar",
            "mods/incompatible.jar",
            "mods/occupied.jar",
            "mods/missing.jar",
        ] {
            assert!(
                validate_cherry_pick_files(&[rejected.to_string()], &classified).is_err(),
                "{rejected} must be rejected"
            );
        }
    }

    #[test]
    fn cherry_pick_resolution_requires_the_complete_dependency_closure() {
        let item = |project: &str, reason: super::super::resolve::PlanReason, installed: bool| {
            super::super::resolve::ResolvedItem {
                canonical_id: CanonicalId::for_project(ProviderId::Modrinth, project),
                provider: ProviderId::Modrinth,
                project_id: project.to_string(),
                kind: ContentKind::Mod,
                version_id: format!("{project}-version"),
                version_number: "1.0.0".to_string(),
                title: project.to_string(),
                file: FileRef {
                    url: format!("https://example.invalid/{project}.jar"),
                    filename: format!("{project}.jar"),
                    sha1: None,
                    sha512: None,
                    size: None,
                    primary: true,
                },
                dependencies: Vec::new(),
                reason,
                already_installed: installed,
                update: false,
            }
        };
        let selected_id = CanonicalId::for_project(ProviderId::Modrinth, "selected");
        let dependency_id = CanonicalId::for_project(ProviderId::Modrinth, "dependency");
        let mut selected = HashSet::from([(selected_id, "selected-version".to_string())]);
        let resolution = super::super::resolve::Resolution {
            items: vec![
                item(
                    "selected",
                    super::super::resolve::PlanReason::Selected,
                    false,
                ),
                item(
                    "dependency",
                    super::super::resolve::PlanReason::Dependency,
                    false,
                ),
            ],
            conflicts: Vec::new(),
        };

        assert!(!cherry_pick_resolution_is_complete(&resolution, &selected));
        selected.insert((dependency_id, "dependency-version".to_string()));
        assert!(cherry_pick_resolution_is_complete(&resolution, &selected));
    }
}
