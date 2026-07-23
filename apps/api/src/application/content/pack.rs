//! Installing a modpack into an instance. The frontend creates the instance
//! first — the pack's loader and Minecraft version are enough to address the
//! create API — and then hands the empty instance here to be filled.
//!
//! Pack files carry sha512 hashes in the index, so once they are on disk we can
//! ask the provider what they are in one batch and record real provenance for
//! every mod, rather than leaving a pack-shaped hole in the manifest.

use super::resolve::resolve_for_execution;
use super::target::{ResolveTarget, instance_target};
use super::{
    ContentApiError, ContentExecutionError, ContentSelection, content_error_response,
    content_execution_error, json_error,
};
use crate::application::{
    InstallQueueContentActionRequest, InstallQueueRequest, InstallQueueStateResponse,
    enqueue_install_owned, enqueue_install_with_dependency_admitted,
};
use crate::state::{AppState, ProducerLease, RequestProducerHandoff, UpdateOperationLease};
use axial_content::{
    CanonicalId, ContentKind, ContentManifest, ContentResolution, FileRef,
    ManagedContentFileName, ManagedPackAvailability, ManagedRemoval, ManifestEntry, PackIndex,
    PackInstallOptions, PendingManifestEntry, ProtectedManagedPaths, ProviderId, VersionIdentity,
    install_pack_files_with_finalize, pick_version, read_pack_index, verified_removable_variants,
};
use axial_fs::{
    Directory, DirectoryCreateOutcome, DirectoryCreateResolution, LeafName,
};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

const MAX_MODPACK_FILE_SELECTIONS: usize = 500;
const MODPACK_FILE_SELECTION_ID_PREFIX: &str = "mpf1-";
const MODPACK_FILE_SELECTION_ID_LEN: usize = MODPACK_FILE_SELECTION_ID_PREFIX.len() + 64;
const MAX_MODPACK_FILE_SELECTION_BYTES: usize =
    MAX_MODPACK_FILE_SELECTIONS * MODPACK_FILE_SELECTION_ID_LEN;
const MAX_MODPACK_FILENAME_CHARS: usize = 160;
const MAX_MODPACK_TITLE_CHARS: usize = 160;

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
#[serde(deny_unknown_fields)]
pub struct ModpackInstallRequest {
    pub instance_id: String,
    pub canonical_id: String,
    #[serde(default)]
    pub version_id: Option<String>,
    #[serde(default)]
    pub selected_file_ids: Vec<String>,
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
    pub selection_id: String,
    pub filename: String,
    pub kind: ContentKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    pub title: String,
    pub identified: bool,
    pub compatible: bool,
    pub installed: bool,
}

#[derive(Debug)]
struct ClassifiedModpackFile {
    path: String,
    option: ModpackFileOption,
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

enum ModpackVersionResolveError {
    Api(ContentApiError),
    Provider(axial_content::ContentError),
}

impl From<ContentApiError> for ModpackVersionResolveError {
    fn from(error: ContentApiError) -> Self {
        Self::Api(error)
    }
}

impl ModpackVersionResolveError {
    fn into_api(self) -> ContentApiError {
        match self {
            Self::Api(error) => error,
            Self::Provider(error) => content_error_response(error),
        }
    }

    fn into_execution(self) -> ContentExecutionError {
        match self {
            Self::Api(error) => error.into(),
            Self::Provider(error) => content_execution_error(error),
        }
    }
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
    resolve_modpack_version_inner(state, canonical_id, version_id)
        .await
        .map_err(ModpackVersionResolveError::into_api)
}

async fn resolve_modpack_version_for_execution(
    state: &AppState,
    canonical_id: &str,
    version_id: Option<&str>,
) -> Result<ResolvedModpackVersion, ContentExecutionError> {
    resolve_modpack_version_inner(state, canonical_id, version_id)
        .await
        .map_err(ModpackVersionResolveError::into_execution)
}

async fn resolve_modpack_version_inner(
    state: &AppState,
    canonical_id: &str,
    version_id: Option<&str>,
) -> Result<ResolvedModpackVersion, ModpackVersionResolveError> {
    let canonical_id = CanonicalId(canonical_id.to_string());
    let detail = state
        .content()
        .detail(&canonical_id)
        .await
        .map_err(ModpackVersionResolveError::Provider)?;
    if detail.content.kind != ContentKind::Modpack {
        return Err(json_error(StatusCode::BAD_REQUEST, "this is not a modpack").into());
    }
    let version = pick_version(&detail.versions, version_id)
        .cloned()
        .ok_or_else(|| {
            json_error(
                StatusCode::NOT_FOUND,
                "this modpack has no installable version",
            )
        })
        .map_err(ModpackVersionResolveError::Api)?;
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
    let archive = download_archive(state, &archive_file, |_| {})
        .await
        .map_err(|error| error.into_parts().0)?;
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
            let component = loader.component_id;
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
        .game_dir()
        .map(Path::to_path_buf)
        .ok_or_else(|| json_error(StatusCode::NOT_FOUND, "instance not found"))?;
    let resolved = resolve_modpack_version(state, canonical_id, version_id).await?;
    let archive_file = resolved.version.primary_file().cloned().ok_or_else(|| {
        json_error(
            StatusCode::NOT_FOUND,
            "this modpack version has no downloadable file",
        )
    })?;
    let archive = download_archive(state, &archive_file, |_| {})
        .await
        .map_err(|error| error.into_parts().0)?;
    let index = read_pack_index(archive.path()).map_err(content_error_response)?;
    validate_selection_surface(&index).map_err(content_error_response)?;
    let identities = identify_modpack_files(state, &index)
        .await
        .map_err(content_error_response)?;
    let classified = classify_modpack_files(
        state,
        &target,
        &game_dir,
        &resolved.canonical_id,
        &resolved.version.id,
        &index,
        &identities,
    )
    .await
    .map_err(content_error_response)?;
    validate_unique_selection_ids(&classified)?;
    let files = classified.into_iter().map(|file| file.option).collect();

    Ok(ModpackFilesPlan {
        canonical_id: resolved.canonical_id,
        version_id: resolved.version.id,
        name: resolved.name,
        minecraft: index.minecraft,
        loader: index
            .loader
            .map(|loader| loader.component_id.short_key().to_string()),
        files,
    })
}

async fn classify_modpack_files(
    state: &AppState,
    target: &ResolveTarget,
    game_dir: &Path,
    pack_id: &CanonicalId,
    version_id: &str,
    index: &PackIndex,
    identities: &HashMap<String, VersionIdentity>,
) -> axial_content::ContentResult<Vec<ClassifiedModpackFile>> {
    let availability = ManagedPackAvailability::capture(game_dir, &index.files)?;
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
                .any(|game| game == &target.resolution().game_version);
            let loader = identity
                .loaders
                .iter()
                .any(|candidate| candidate == &target.resolution().loader);
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

    Ok(classify_modpack_file_options(
        &availability,
        pack_id,
        version_id,
        index,
        identities,
        &identity_versions,
    ))
}

async fn identify_modpack_files(
    state: &AppState,
    index: &PackIndex,
) -> axial_content::ContentResult<HashMap<String, VersionIdentity>> {
    let hashes: Vec<String> = index
        .files
        .iter()
        .filter_map(|file| file.sha512.clone())
        .collect();
    if hashes.is_empty() {
        Ok(HashMap::new())
    } else {
        state.content().identify(&hashes).await
    }
}

fn classify_modpack_file_options(
    availability: &ManagedPackAvailability,
    pack_id: &CanonicalId,
    version_id: &str,
    index: &PackIndex,
    identities: &HashMap<String, VersionIdentity>,
    identity_versions: &PackCompatibilityMap,
) -> Vec<ClassifiedModpackFile> {
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
        let installed = availability.contains(file);
        let filename =
            bounded_display_text(file.filename(), MAX_MODPACK_FILENAME_CHARS, "Pack file");
        let title = identity
            .and_then(|identity| {
                let project_id = CanonicalId::for_project(identity.provider, &identity.project_id);
                identity_versions
                    .get(&(project_id.as_str().to_string(), identity.version_id.clone()))
                    .map(|compatibility| compatibility.title.clone())
                    .or_else(|| identity.title.clone())
            })
            .unwrap_or_else(|| filename.clone());
        files.push(ClassifiedModpackFile {
            path: file.path.clone(),
            option: ModpackFileOption {
                selection_id: modpack_file_selection_id(pack_id, version_id, &file.path),
                filename,
                kind,
                size: file.size,
                title: bounded_display_text(&title, MAX_MODPACK_TITLE_CHARS, "Pack file"),
                identified: identity.is_some(),
                compatible,
                installed,
            },
        });
    }
    files.sort_by_key(|file| file.option.title.to_lowercase());
    files
}

pub(crate) async fn queue_modpack_install(
    state: &AppState,
    request: ModpackInstallRequest,
    handoff: RequestProducerHandoff,
) -> Result<InstallQueueStateResponse, ContentApiError> {
    validate_modpack_file_selection_ids(&request.selected_file_ids)?;
    reject_cherry_pick_overrides(&request)?;
    let resolved =
        resolve_modpack_version(state, &request.canonical_id, request.version_id.as_deref())
            .await?;
    enqueue_install_owned(
        state,
        pinned_modpack_queue_request(
            request,
            resolved.canonical_id,
            resolved.version.id,
            resolved.name,
        ),
        handoff,
    )
    .await
}

pub(crate) async fn queue_modpack_install_after_admitted(
    state: &AppState,
    request: ModpackInstallRequest,
    prerequisite_queue_id: Option<String>,
    setup_cleanup: Option<crate::state::SetupInstanceCleanup>,
    producer: ProducerLease,
    update_admission: UpdateOperationLease,
) -> Result<InstallQueueStateResponse, ContentApiError> {
    validate_modpack_file_selection_ids(&request.selected_file_ids)?;
    reject_cherry_pick_overrides(&request)?;
    let resolved =
        resolve_modpack_version(state, &request.canonical_id, request.version_id.as_deref())
            .await?;
    enqueue_install_with_dependency_admitted(
        state,
        pinned_modpack_queue_request(
            request,
            resolved.canonical_id,
            resolved.version.id,
            resolved.name,
        ),
        prerequisite_queue_id,
        setup_cleanup,
        producer,
        update_admission,
    )
    .await
}

fn pinned_modpack_queue_request(
    request: ModpackInstallRequest,
    canonical_id: CanonicalId,
    version_id: String,
    name: String,
) -> InstallQueueRequest {
    let include_overrides = request.selected_file_ids.is_empty() && request.include_overrides;
    let label = if request.selected_file_ids.is_empty() {
        format!("Setting up {name}")
    } else {
        format!(
            "Adding {} files from {}",
            request.selected_file_ids.len(),
            name
        )
    };
    InstallQueueRequest::Content {
        instance_id: request.instance_id,
        label,
        action: InstallQueueContentActionRequest::Modpack {
            canonical_id: canonical_id.as_str().to_string(),
            version_id,
            selected_file_ids: request.selected_file_ids,
            include_overrides,
        },
    }
}

pub(crate) async fn execute_modpack_install<F, G>(
    state: &AppState,
    request: ModpackInstallRequest,
    mut on_progress: F,
    mut on_download_fact: G,
) -> Result<ModpackInstallResponse, ContentExecutionError>
where
    F: FnMut(axial_minecraft::DownloadProgress),
    G: FnMut(axial_minecraft::download::ExecutionDownloadFact),
{
    validate_modpack_file_selection_ids(&request.selected_file_ids)?;
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
    let _lifecycle_guard =
        super::lock_instance_for_content_mutation(state, &request.instance_id).await?;
    if state
        .sessions()
        .has_active_instance(&request.instance_id)
        .await
    {
        return Err(json_error(
            StatusCode::CONFLICT,
            "cannot install a modpack while the instance is running; stop the game first",
        )
        .into());
    }
    let target = instance_target(state, &request.instance_id).await?;
    let game_dir = target
        .game_dir()
        .map(Path::to_path_buf)
        .ok_or_else(|| json_error(StatusCode::NOT_FOUND, "instance not found"))?;

    let resolved = resolve_modpack_version_for_execution(
        state,
        &request.canonical_id,
        request.version_id.as_deref(),
    )
    .await?;
    let archive_file = resolved.version.primary_file().cloned().ok_or_else(|| {
        json_error(
            StatusCode::NOT_FOUND,
            "this modpack version has no downloadable file",
        )
    })?;

    let archive = download_archive(state, &archive_file, &mut on_download_fact).await?;
    let preview = read_pack_index(archive.path()).map_err(content_execution_error)?;
    let selected_paths = if request.selected_file_ids.is_empty() {
        Vec::new()
    } else {
        validate_selection_surface(&preview).map_err(content_execution_error)?;
        let identities = identify_modpack_files(state, &preview)
            .await
            .map_err(content_execution_error)?;
        let classified = classify_modpack_files(
            state,
            &target,
            &game_dir,
            &resolved.canonical_id,
            &resolved.version.id,
            &preview,
            &identities,
        )
        .await
        .map_err(content_execution_error)?;
        let selected_paths = resolve_selected_paths(&request.selected_file_ids, &classified)?;
        validate_cherry_pick_dependencies(
            state,
            &target,
            &game_dir,
            &preview,
            &selected_paths,
            &identities,
        )
        .await?;
        selected_paths
    };
    let preview_files: Vec<axial_content::PackFile> = preview
        .files
        .iter()
        .filter(|file| {
            selected_paths.is_empty() || selected_paths.iter().any(|path| path == &file.path)
        })
        .cloned()
        .collect();
    let mut prepared_manifest = prepare_pack_manifest(
        state,
        &game_dir,
        &preview_files,
        &resolved.canonical_id,
        &resolved.name,
        &resolved.version,
        selected_paths.is_empty(),
    )
    .await?;
    let identified = prepared_manifest.entries.len();
    let protected_paths = preview_files
        .iter()
        .filter(|file| file.kind().is_some())
        .map(|file| file.path.clone())
        .collect::<Vec<_>>();
    let protected_paths =
        ProtectedManagedPaths::new(&protected_paths).map_err(content_execution_error)?;
    let stale_files = verified_stale_pack_files(
        &game_dir,
        &prepared_manifest.stale_entries,
        &protected_paths,
    )
    .map_err(content_execution_error)?;
    let stale_guarded_paths = stale_files
        .iter()
        .map(|removal| removal.relative_path().to_string())
        .collect::<Vec<_>>();
    let game_directory = state
        .root_session()
        .admit_absolute_directory(&game_dir)
        .map_err(|error| {
            content_execution_error(axial_content::ContentError::Io(error))
        })?;
    let _mutation = state.admit_managed_artifact_mutation().map_err(|error| {
        content_execution_error(axial_content::ContentError::Io(std::io::Error::other(
            error.to_string(),
        )))
    })?;
    let install = install_pack_files_with_finalize(
        &game_dir,
        &game_directory,
        archive.path(),
        PackInstallOptions {
            selected_paths: &selected_paths,
            additional_guarded_paths: &stale_guarded_paths,
            include_overrides: request.include_overrides,
        },
        &mut on_progress,
        &mut on_download_fact,
        |report, finalizer| {
            prepared_manifest.materialize(&report.installed)?;
            finalizer.stage_removals(&stale_files)?;
            Ok(std::mem::take(&mut prepared_manifest.manifest))
        },
    )
    .await;
    let report = install.map_err(content_execution_error)?;

    let mismatch = mismatch_notice(
        &target.resolution().loader,
        &target.resolution().game_version,
        report
            .index
            .loader
            .as_ref()
            .map(|loader| loader.component_id.short_key()),
        &report.index.minecraft,
    );

    Ok(ModpackInstallResponse {
        instance_id: request.instance_id,
        name: report.index.name,
        version: report.index.version,
        minecraft: report.index.minecraft,
        loader: report
            .index
            .loader
            .map(|loader| loader.component_id.short_key().to_string()),
        file_count: report.installed.len(),
        overrides_applied: report.overrides_applied,
        identified_count: identified,
        mismatch,
    })
}

fn reject_cherry_pick_overrides(request: &ModpackInstallRequest) -> Result<(), ContentApiError> {
    if !request.selected_file_ids.is_empty() && request.include_overrides {
        return Err(json_error(
            StatusCode::BAD_REQUEST,
            "modpack overrides cannot be applied with selected files",
        ));
    }
    Ok(())
}

fn resolve_selected_paths(
    selected_file_ids: &[String],
    classified: &[ClassifiedModpackFile],
) -> Result<Vec<String>, ContentApiError> {
    validate_unique_selection_ids(classified)?;
    let mut paths_by_id = HashMap::with_capacity(classified.len());
    for file in classified {
        paths_by_id.insert(file.option.selection_id.as_str(), file);
    }
    let mut selected_paths = Vec::with_capacity(selected_file_ids.len());
    for selection_id in selected_file_ids {
        let Some(file) = paths_by_id.get(selection_id.as_str()) else {
            return Err(cherry_pick_files_changed());
        };
        if !file.option.identified || !file.option.compatible || file.option.installed {
            return Err(cherry_pick_files_changed());
        }
        selected_paths.push(file.path.clone());
    }
    Ok(selected_paths)
}

fn validate_unique_selection_ids(
    classified: &[ClassifiedModpackFile],
) -> Result<(), ContentApiError> {
    let unique = classified
        .iter()
        .map(|file| file.option.selection_id.as_str())
        .collect::<HashSet<_>>();
    if unique.len() != classified.len() {
        return Err(json_error(
            StatusCode::CONFLICT,
            "modpack file identities are ambiguous; review the pack and try again",
        ));
    }
    Ok(())
}

fn validate_selection_surface(index: &PackIndex) -> axial_content::ContentResult<()> {
    if index
        .files
        .iter()
        .filter(|file| file.kind().is_some())
        .count()
        > MAX_MODPACK_FILE_SELECTIONS
    {
        return Err(axial_content::ContentError::ProviderMetadataInvalid(
            "modpack has too many files for selective installation".to_string(),
        ));
    }
    Ok(())
}

pub(crate) fn validate_modpack_file_selection_ids(
    selected_file_ids: &[String],
) -> Result<(), ContentApiError> {
    let aggregate_bytes = selected_file_ids
        .iter()
        .try_fold(0usize, |total, selection_id| {
            total.checked_add(selection_id.len())
        })
        .unwrap_or(usize::MAX);
    let unique: HashSet<&str> = selected_file_ids.iter().map(String::as_str).collect();
    if selected_file_ids.len() > MAX_MODPACK_FILE_SELECTIONS
        || aggregate_bytes > MAX_MODPACK_FILE_SELECTION_BYTES
        || unique.len() != selected_file_ids.len()
        || selected_file_ids
            .iter()
            .any(|selection_id| !valid_modpack_file_selection_id(selection_id))
    {
        return Err(json_error(
            StatusCode::BAD_REQUEST,
            "selected_file_ids is invalid",
        ));
    }
    Ok(())
}

fn valid_modpack_file_selection_id(selection_id: &str) -> bool {
    selection_id.len() == MODPACK_FILE_SELECTION_ID_LEN
        && selection_id
            .strip_prefix(MODPACK_FILE_SELECTION_ID_PREFIX)
            .is_some_and(|digest| {
                digest
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            })
}

fn modpack_file_selection_id(
    pack_id: &CanonicalId,
    version_id: &str,
    normalized_path: &str,
) -> String {
    let mut digest = Sha256::new();
    update_selection_id_frame(&mut digest, b"axial.modpack-file-selection.v1");
    update_selection_id_frame(&mut digest, pack_id.as_str().as_bytes());
    update_selection_id_frame(&mut digest, version_id.as_bytes());
    update_selection_id_frame(&mut digest, normalized_path.as_bytes());
    format!("{MODPACK_FILE_SELECTION_ID_PREFIX}{:x}", digest.finalize())
}

fn update_selection_id_frame(digest: &mut Sha256, value: &[u8]) {
    digest.update((value.len() as u64).to_be_bytes());
    digest.update(value);
}

fn bounded_display_text(value: &str, max_chars: usize, fallback: &str) -> String {
    let normalized = value
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(max_chars)
        .collect::<String>();
    if normalized.is_empty() {
        fallback.to_string()
    } else {
        normalized
    }
}

fn cherry_pick_files_changed() -> ContentApiError {
    json_error(
        StatusCode::CONFLICT,
        "selected modpack files are no longer compatible or available; review them and try again",
    )
}

async fn validate_cherry_pick_dependencies(
    state: &AppState,
    target: &ResolveTarget,
    game_dir: &Path,
    index: &PackIndex,
    selected_paths: &[String],
    identities: &HashMap<String, VersionIdentity>,
) -> Result<(), ContentExecutionError> {
    let files: HashMap<&str, &axial_content::PackFile> = index
        .files
        .iter()
        .map(|file| (file.path.as_str(), file))
        .collect();
    let mut selections = Vec::with_capacity(selected_paths.len());
    let mut selected_versions = HashSet::with_capacity(selected_paths.len());
    for path in selected_paths {
        let Some(file) = files.get(path.as_str()) else {
            return Err(cherry_pick_conflict().into());
        };
        let Some(kind) = file.kind() else {
            return Err(cherry_pick_conflict().into());
        };
        let Some(identity) = file.sha512.as_ref().and_then(|hash| identities.get(hash)) else {
            return Err(cherry_pick_conflict().into());
        };
        let canonical_id = CanonicalId::for_project(identity.provider, &identity.project_id);
        if !selected_versions.insert((canonical_id.clone(), identity.version_id.clone())) {
            return Err(cherry_pick_conflict().into());
        }
        selections.push(ContentSelection {
            canonical_id: canonical_id.as_str().to_string(),
            kind,
            version_id: Some(identity.version_id.clone()),
        });
    }

    let (manifest, live_content) =
        super::load_ambient_content_snapshot(game_dir.to_path_buf(), None)
            .await
            .map_err(content_error_response)?;
    let resolution = resolve_for_execution(
        state,
        target.resolution(),
        &selections,
        &live_content,
    )
    .await?;
    if !cherry_pick_resolution_is_complete(&resolution, &selected_versions) {
        return Err(cherry_pick_conflict().into());
    }
    Ok(())
}

fn cherry_pick_resolution_is_complete(
    resolution: &ContentResolution,
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
async fn download_archive<G>(
    state: &AppState,
    file: &FileRef,
    mut on_download_fact: G,
) -> Result<ScratchArchive, ContentExecutionError>
where
    G: FnMut(axial_minecraft::download::ExecutionDownloadFact),
{
    let instances_directory = state
        .root_session()
        .prepare_instances_directory()
        .map_err(|error| content_execution_error(axial_content::ContentError::Io(error)))?;
    let scratch_directory = open_or_create_scratch_directory(instances_directory)
        .map_err(|error| content_execution_error(axial_content::ContentError::Io(error)))?;
    let archive_name = format!(
        ".axial-pack-{}-{}.mrpack",
        std::process::id(),
        uuid::Uuid::new_v4()
    );
    let archive = ScratchArchive::new(
        state
            .config()
            .paths()
            .instances_dir()
            .join(".axial-content-scratch")
            .join(&archive_name),
    );
    let expected = axial_minecraft::download::VerifiedContentIntegrity {
        size: file.size,
        sha1: file.sha1.clone(),
        sha512: file.sha512.clone(),
    };
    match axial_minecraft::download::download_owned_verified_content_to_staging(
        state.content().client(),
        &file.url,
        &scratch_directory,
        &archive_name,
        &expected,
    )
    .await
    {
        Ok(staged) => {
            let report = staged
                .publish_create_new(&scratch_directory, &archive_name)
                .map_err(|error| {
                    content_execution_error(axial_content::ContentError::Io(
                        std::io::Error::other(error),
                    ))
                })?;
            for fact in report.facts {
                on_download_fact(fact);
            }
        }
        Err(error) => {
            for fact in &error.facts {
                on_download_fact(fact.clone());
            }
            return Err(content_execution_error(
                axial_content::ContentError::Download(error),
            ));
        }
    }
    Ok(archive)
}

fn open_or_create_scratch_directory(parent: Directory) -> std::io::Result<Directory> {
    let name = LeafName::new(".axial-content-scratch")
        .expect("fixed content scratch directory name is valid");
    match parent.open_directory(&name) {
        Ok(directory) => return Ok(directory),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    match parent.create_directory(&name) {
        DirectoryCreateOutcome::Created(directory) => Ok(directory),
        DirectoryCreateOutcome::NoEffect(error)
            if error.kind() == std::io::ErrorKind::AlreadyExists =>
        {
            parent.open_directory(&name)
        }
        DirectoryCreateOutcome::NoEffect(error) => Err(error),
        DirectoryCreateOutcome::CreatedUnclassified {
            error,
            preservation,
        } => {
            let kind = error.kind();
            let message = error.to_string();
            if preservation.acknowledge_preserved().is_err() {
                std::process::abort();
            }
            Err(std::io::Error::new(kind, message))
        }
        DirectoryCreateOutcome::AppliedUnverified(obligation) => match obligation.reconcile() {
            DirectoryCreateResolution::Created(directory) => Ok(directory),
            DirectoryCreateResolution::Indeterminate(_) => std::process::abort(),
        },
    }
}

struct PreparedPackManifest {
    manifest: ContentManifest,
    entries: Vec<PendingManifestEntry>,
    stale_entries: Vec<ManifestEntry>,
}

fn materialize_pending_pack_entry(
    pending: &PendingManifestEntry,
    file: &axial_content::PackFile,
) -> axial_content::ContentResult<ManifestEntry> {
    let size = file.size.filter(|size| *size > 0).ok_or_else(|| {
        axial_content::ContentError::ProviderMetadataInvalid(
            "identified modpack content has no positive authenticated size".to_string(),
        )
    })?;
    let expected_path = format!(
        "{}/{}",
        pending
            .kind()
            .install_subdir()
            .expect("pending entries are file-owning"),
        pending.filename().as_str()
    );
    if file.sha512.as_deref() != Some(pending.sha512())
        || file.kind() != Some(pending.kind())
        || file.path != expected_path
    {
        return Err(axial_content::ContentError::ProviderMetadataInvalid(
            "identified modpack content changed before manifest finalization".to_string(),
        ));
    }
    pending.clone().materialize(size)
}

impl PreparedPackManifest {
    fn materialize(
        &mut self,
        installed: &[axial_content::PackFile],
    ) -> axial_content::ContentResult<()> {
        let required_hashes = self
            .entries
            .iter()
            .map(PendingManifestEntry::sha512)
            .collect::<HashSet<_>>();
        let mut installed_by_hash = HashMap::new();
        for file in installed.iter().filter(|file| file.kind().is_some()) {
            let Some(hash) = file.sha512.as_deref() else {
                continue;
            };
            if !required_hashes.contains(hash) {
                continue;
            }
            if installed_by_hash.insert(hash, file).is_some() {
                return Err(axial_content::ContentError::ProviderMetadataInvalid(
                    "authenticated modpack content repeats an ownership digest".to_string(),
                ));
            }
        }
        if installed_by_hash.len() < self.entries.len() {
            return Err(axial_content::ContentError::ProviderMetadataInvalid(
                "authenticated modpack content cardinality changed before finalization"
                    .to_string(),
            ));
        }

        let mut materialized = Vec::with_capacity(self.entries.len());
        for pending in &self.entries {
            let file = installed_by_hash.get(pending.sha512()).ok_or_else(|| {
                axial_content::ContentError::ProviderMetadataInvalid(
                    "identified modpack content changed before manifest finalization".to_string(),
                )
            })?;
            materialized.push(materialize_pending_pack_entry(pending, file)?);
        }

        let mut next_manifest = self.manifest.clone();
        let displaced = next_manifest.try_upsert_batch(materialized)?;
        next_manifest.validate_provider_projection()?;
        let mut next_stale_entries = self.stale_entries.clone();
        let mut stale_ids = next_stale_entries
            .iter()
            .map(|entry| entry.canonical_id().clone())
            .collect::<HashSet<_>>();
        for previous in displaced {
            if stale_ids.insert(previous.canonical_id().clone()) {
                next_stale_entries.push(previous);
            }
        }
        self.manifest = next_manifest;
        self.stale_entries = next_stale_entries;
        self.entries.clear();
        Ok(())
    }
}

/// Resolve pack-index hashes into provider identities before filesystem mutation.
/// Exact sizes are materialized from authenticated downloads in the transaction
/// finalizer; files the provider does not recognize remain unmanaged.
async fn prepare_pack_manifest(
    state: &AppState,
    game_dir: &Path,
    installed: &[axial_content::PackFile],
    pack_id: &CanonicalId,
    pack_title: &str,
    version: &axial_content::ContentVersion,
    record_pack_root: bool,
) -> Result<PreparedPackManifest, ContentExecutionError> {
    let mut manifest = ContentManifest::load(game_dir).map_err(content_error_response)?;
    let mut stale_entries = Vec::new();
    let mut stale_ids = HashSet::new();
    let mut entries = Vec::new();

    // The pack itself: what this instance was built from, so an update knows
    // where it came from.
    if record_pack_root {
        let entry = ManifestEntry::provenance(
            pack_id.clone(),
            ProviderId::Modrinth,
            pack_id.project_id().to_string(),
            version.id.clone(),
            Some(pack_title.to_string()),
        )
        .map_err(content_execution_error)?;
        manifest
            .validate_provider_entry(&entry)
            .map_err(content_execution_error)?;
        if let Some(previous) = manifest.try_upsert(entry).map_err(content_execution_error)? {
            stale_ids.insert(previous.canonical_id().clone());
            stale_entries.push(previous);
        }
    }
    let manifest_indexes = manifest
        .entries()
        .iter()
        .enumerate()
        .map(|(index, entry)| (entry.canonical_id().clone(), index))
        .collect::<HashMap<_, _>>();
    let by_hash = group_pack_files_by_sha512(installed);

    if !by_hash.is_empty() {
        let hashes: Vec<String> = by_hash.keys().cloned().collect();
        let resolved = state
            .content()
            .identify(&hashes)
            .await
            .map_err(content_execution_error)?;
        reject_duplicate_pack_projects(
            &resolved,
            &by_hash,
            record_pack_root.then_some(pack_id),
        )?;
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
            let canonical_id = CanonicalId::for_project(identity.provider, &identity.project_id);
            let title = titles
                .get(&canonical_id)
                .cloned()
                .or(identity.title.clone());
            if file.size == Some(0) {
                return Err(content_execution_error(
                    axial_content::ContentError::ProviderMetadataInvalid(
                        "identified modpack content has a zero declared size".to_string(),
                    ),
                ));
            }
            let filename = axial_content::ManagedContentFileName::new_exact(file.filename())
                .map_err(|_| {
                    content_execution_error(
                        axial_content::ContentError::ProviderMetadataInvalid(
                            "identified modpack content has an invalid filename".to_string(),
                        ),
                    )
                })?;
            let sha512 = file.sha512.clone().ok_or_else(|| {
                content_execution_error(
                    axial_content::ContentError::ProviderMetadataInvalid(
                        "identified modpack content has no exact SHA-512 digest".to_string(),
                    ),
                )
            })?;
            if let Some(existing) = manifest_indexes
                .get(&canonical_id)
                .map(|index| &manifest.entries()[*index])
            {
                if (existing.kind() != kind
                    || existing.managed_filename() != Some(&filename))
                    && stale_ids.insert(existing.canonical_id().clone())
                {
                    stale_entries.push(existing.clone());
                }
            }
            entries.push(PendingManifestEntry::managed_file(
                canonical_id,
                identity.provider,
                identity.project_id,
                identity.version_id,
                kind,
                filename,
                sha512,
                identity.dependencies,
                title,
            ).map_err(content_execution_error)?);
        }
    }
    manifest
        .validate_provider_pending_projection(&entries)
        .map_err(content_execution_error)?;

    Ok(PreparedPackManifest {
        manifest,
        entries,
        stale_entries,
    })
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

fn reject_duplicate_pack_projects(
    resolved: &HashMap<String, VersionIdentity>,
    grouped: &HashMap<String, Vec<&axial_content::PackFile>>,
    reserved_pack_id: Option<&CanonicalId>,
) -> Result<(), ContentExecutionError> {
    let mut projects = HashSet::new();
    for (hash, identity) in resolved {
        let Some(files) = grouped.get(hash) else {
            continue;
        };
        if files.len() > 1 {
            return Err(content_execution_error(
                axial_content::ContentError::ProviderMetadataInvalid(
                    "modpack repeats the same identified content at multiple paths".to_string(),
                ),
            ));
        }
        let Some(file) = files.first() else { continue };
        if file.kind().is_none() {
            continue;
        }
        let canonical_id = CanonicalId::for_project(identity.provider, &identity.project_id);
        if reserved_pack_id == Some(&canonical_id) {
            return Err(content_execution_error(
                axial_content::ContentError::ProviderMetadataInvalid(
                    "modpack provenance collides with an identified managed member".to_string(),
                ),
            ));
        }
        if !projects.insert(canonical_id) {
            return Err(content_execution_error(
                axial_content::ContentError::ProviderMetadataInvalid(
                    "modpack contains multiple managed files for the same project".to_string(),
                ),
            ));
        }
    }
    Ok(())
}

fn verified_stale_pack_files(
    game_dir: &Path,
    stale_entries: &[ManifestEntry],
    protected_paths: &ProtectedManagedPaths,
) -> axial_content::ContentResult<Vec<ManagedRemoval>> {
    let mut files = Vec::new();
    for entry in stale_entries {
        files.extend(verified_removable_variants(
            game_dir,
            entry,
            protected_paths,
        )?);
    }
    files.sort_by(|left, right| left.relative_path().cmp(right.relative_path()));
    files.dedup_by(|left, right| left == right);
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

#[cfg(test)]
mod tests {
    use super::super::ContentExecutionFailureKind;
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
    fn execution_modpack_resolution_preserves_closed_provider_failure_kinds() {
        let cases = [
            (
                axial_content::ContentError::DownloadPreparation(
                    "prepare modpack download".to_string(),
                ),
                ContentExecutionFailureKind::NetworkFailure,
            ),
            (
                axial_content::ContentError::Status {
                    status: reqwest::StatusCode::SERVICE_UNAVAILABLE,
                    context: "resolve modpack version".to_string(),
                },
                ContentExecutionFailureKind::ProviderFailure,
            ),
            (
                axial_content::ContentError::ProviderMetadataInvalid(
                    "invalid modpack metadata".to_string(),
                ),
                ContentExecutionFailureKind::MetadataInvalid,
            ),
        ];

        for (error, expected) in cases {
            let (_, failure_kind) = ModpackVersionResolveError::Provider(error)
                .into_execution()
                .into_parts();
            assert_eq!(failure_kind, Some(expected));
        }
    }

    #[test]
    fn execution_modpack_resolution_leaves_local_conflicts_unclassified() {
        let (_, failure_kind) = ModpackVersionResolveError::Api(json_error(
            StatusCode::CONFLICT,
            "modpack selection changed",
        ))
        .into_execution()
        .into_parts();

        assert_eq!(failure_kind, None);
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
        let resolved = HashMap::from([(
            "shared-hash".to_string(),
            VersionIdentity {
                provider: ProviderId::Modrinth,
                project_id: "identified-project".to_string(),
                version_id: "identified-version".to_string(),
                game_versions: Vec::new(),
                loaders: Vec::new(),
                dependencies: Vec::new(),
                title: None,
            },
        )]);
        assert_eq!(grouped["shared-hash"].len(), 2);
        assert!(
            reject_duplicate_pack_projects(&HashMap::new(), &grouped, None).is_ok(),
            "unidentified duplicate content remains unmanaged"
        );
        let ((status, _), failure_kind) = reject_duplicate_pack_projects(&resolved, &grouped, None)
            .expect_err("one manifest entry cannot safely own two paths")
            .into_parts();
        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert_eq!(
            failure_kind,
            Some(ContentExecutionFailureKind::MetadataInvalid)
        );
    }

    #[test]
    fn duplicate_pack_projects_are_rejected_before_manifest_finalization() {
        let file = |path: &str, hash: &str| axial_content::PackFile {
            path: path.to_string(),
            url: format!("https://example.invalid/{path}"),
            sha1: None,
            sha512: Some(hash.to_string()),
            size: Some(42),
        };
        let installed = vec![
            file("mods/first.jar", "first-hash"),
            file("mods/second.jar", "second-hash"),
        ];
        let identity = |version_id: &str| VersionIdentity {
            provider: ProviderId::Modrinth,
            project_id: "shared-project".to_string(),
            version_id: version_id.to_string(),
            game_versions: Vec::new(),
            loaders: Vec::new(),
            dependencies: Vec::new(),
            title: None,
        };
        let resolved = HashMap::from([
            ("first-hash".to_string(), identity("first-version")),
            ("second-hash".to_string(), identity("second-version")),
        ]);
        let grouped = group_pack_files_by_sha512(&installed);

        let ((status, _), failure_kind) = reject_duplicate_pack_projects(&resolved, &grouped, None)
            .expect_err("one canonical project cannot safely own two pack files")
            .into_parts();
        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert_eq!(
            failure_kind,
            Some(ContentExecutionFailureKind::MetadataInvalid)
        );
    }

    #[test]
    fn pack_root_cannot_be_replaced_by_an_identified_member() {
        let hash = "a".repeat(128);
        let installed = vec![axial_content::PackFile {
            path: "mods/member.jar".to_string(),
            url: "https://example.invalid/member.jar".to_string(),
            sha1: None,
            sha512: Some(hash.clone()),
            size: Some(42),
        }];
        let resolved = HashMap::from([(
            hash,
            VersionIdentity {
                provider: ProviderId::Modrinth,
                project_id: "pack-project".to_string(),
                version_id: "member-version".to_string(),
                game_versions: Vec::new(),
                loaders: Vec::new(),
                dependencies: Vec::new(),
                title: None,
            },
        )]);
        let grouped = group_pack_files_by_sha512(&installed);
        let pack_id = CanonicalId::for_project(ProviderId::Modrinth, "pack-project");

        let ((status, _), failure_kind) =
            reject_duplicate_pack_projects(&resolved, &grouped, Some(&pack_id))
                .expect_err("pack provenance must not be displaced by a member")
                .into_parts();

        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert_eq!(
            failure_kind,
            Some(ContentExecutionFailureKind::MetadataInvalid)
        );
    }

    fn prepared_pack_entry(hash: &str) -> PendingManifestEntry {
        prepared_pack_entry_for("project", "managed.jar", hash)
    }

    fn prepared_pack_entry_for(
        project: &str,
        filename: &str,
        hash: &str,
    ) -> PendingManifestEntry {
        PendingManifestEntry::managed_file(
            CanonicalId::for_project(ProviderId::Modrinth, project),
            ProviderId::Modrinth,
            project.to_string(),
            "version".to_string(),
            ContentKind::Mod,
            ManagedContentFileName::new_exact(filename).unwrap(),
            hash.to_string(),
            Vec::new(),
            Some("Managed".to_string()),
        )
        .expect("pending manifest entry")
    }

    fn authenticated_pack_file(hash: &str, size: u64) -> axial_content::PackFile {
        authenticated_pack_file_for("managed.jar", hash, size)
    }

    fn authenticated_pack_file_for(
        filename: &str,
        hash: &str,
        size: u64,
    ) -> axial_content::PackFile {
        axial_content::PackFile {
            path: format!("mods/{filename}"),
            url: format!("https://example.invalid/{filename}"),
            sha1: None,
            sha512: Some(hash.to_string()),
            size: Some(size),
        }
    }

    #[test]
    fn p00_b11_contract_pack_manifest_derives_missing_size_from_authenticated_bytes() {
        let hash = "a".repeat(128);
        let mut prepared = PreparedPackManifest {
            manifest: ContentManifest::default(),
            entries: vec![prepared_pack_entry(&hash)],
            stale_entries: Vec::new(),
        };

        prepared
            .materialize(&[authenticated_pack_file(&hash, 42)])
            .expect("authenticated size should materialize ownership");

        let entry = prepared
            .manifest
            .find(&CanonicalId::for_project(ProviderId::Modrinth, "project"))
            .expect("materialized entry");
        assert_eq!(entry.size(), Some(42));
        assert_eq!(entry.sha512(), Some(hash.as_str()));
    }

    #[test]
    fn p00_b11_contract_pack_manifest_rejects_zero_or_mismatched_authenticated_evidence() {
        let hash = "a".repeat(128);
        for installed in [
            authenticated_pack_file(&hash, 0),
            authenticated_pack_file(&"b".repeat(128), 42),
        ] {
            let mut prepared = PreparedPackManifest {
                manifest: ContentManifest::default(),
                entries: vec![prepared_pack_entry(&hash)],
                stale_entries: Vec::new(),
            };

            assert!(prepared.materialize(&[installed]).is_err());
            assert!(prepared.manifest.is_empty());
        }
    }

    #[test]
    fn sha1_only_managed_path_remains_unmanaged_during_manifest_materialization() {
        let mut prepared = PreparedPackManifest {
            manifest: ContentManifest::default(),
            entries: Vec::new(),
            stale_entries: Vec::new(),
        };
        let installed = axial_content::PackFile {
            path: "mods/sha1-only.jar".to_string(),
            url: "https://example.invalid/sha1-only.jar".to_string(),
            sha1: Some("a".repeat(40)),
            sha512: None,
            size: Some(42),
        };

        prepared
            .materialize(&[installed])
            .expect("unidentified SHA1-only content remains unmanaged");

        assert!(prepared.manifest.is_empty());
        assert!(prepared.entries.is_empty());
        assert!(prepared.stale_entries.is_empty());
    }

    #[test]
    fn duplicate_unknown_sha512_files_remain_unmanaged_during_materialization() {
        let mut prepared = PreparedPackManifest {
            manifest: ContentManifest::default(),
            entries: Vec::new(),
            stale_entries: Vec::new(),
        };
        let hash = "a".repeat(128);
        let installed = [
            authenticated_pack_file_for("first.jar", &hash, 42),
            authenticated_pack_file_for("second.jar", &hash, 42),
        ];

        prepared
            .materialize(&installed)
            .expect("duplicate unidentified content remains unmanaged");

        assert!(prepared.manifest.is_empty());
        assert!(prepared.entries.is_empty());
        assert!(prepared.stale_entries.is_empty());
    }

    #[test]
    fn pack_manifest_materialization_is_atomic() {
        let first_hash = "a".repeat(128);
        let second_hash = "b".repeat(128);
        let mut prepared = PreparedPackManifest {
            manifest: ContentManifest::default(),
            entries: vec![
                prepared_pack_entry_for("first", "first.jar", &first_hash),
                prepared_pack_entry_for("second", "second.jar", &second_hash),
            ],
            stale_entries: Vec::new(),
        };

        assert!(
            prepared
                .materialize(&[
                    authenticated_pack_file_for("first.jar", &first_hash, 42),
                    authenticated_pack_file_for("second.jar", &second_hash, 0),
                ])
                .is_err()
        );
        assert!(prepared.manifest.is_empty());
        assert_eq!(prepared.entries.len(), 2);
        assert!(prepared.stale_entries.is_empty());
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
    fn malformed_pack_index_is_typed_without_classifying_generic_invalid_errors() {
        let path = std::env::temp_dir().join(format!(
            "axial-malformed-pack-index-test-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::write(&path, b"not a zip archive").expect("write malformed pack");

        let error = read_pack_index(&path).expect_err("malformed pack must fail");
        assert!(matches!(
            &error,
            axial_content::ContentError::ProviderMetadataInvalid(_)
        ));
        let ((status, _), failure_kind) = content_execution_error(error).into_parts();
        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert_eq!(
            failure_kind,
            Some(ContentExecutionFailureKind::MetadataInvalid)
        );

        let ((status, _), failure_kind) =
            content_execution_error(axial_content::ContentError::Invalid(
                "a modpack destination is already occupied".to_string(),
            ))
            .into_parts();
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(failure_kind, None);

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn provider_identification_failures_remain_typed_for_execution() {
        let (_, failure_kind) = content_execution_error(axial_content::ContentError::Status {
            status: reqwest::StatusCode::SERVICE_UNAVAILABLE,
            context: "identify modpack files".to_string(),
        })
        .into_parts();

        assert_eq!(
            failure_kind,
            Some(ContentExecutionFailureKind::ProviderFailure)
        );
    }

    #[test]
    fn local_parse_failures_remain_unclassified_for_execution() {
        let parse_error =
            serde_json::from_slice::<serde_json::Value>(b"{").expect_err("invalid local JSON");
        let (_, failure_kind) =
            content_execution_error(axial_content::ContentError::Parse(parse_error)).into_parts();

        assert_eq!(failure_kind, None);
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
                sha512: Some(axial_content::sha512_file(&path).expect("tracked hash")),
                size: Some(b"tracked bytes".len() as u64),
                primary: true,
            },
            Vec::new(),
            None,
        )
        .expect("valid managed entry");

        std::fs::write(&path, b"user replacement").expect("replace tracked file");
        assert!(
            verified_stale_pack_files(
                &root,
                &[entry],
                &ProtectedManagedPaths::default(),
            )
            .is_err()
        );
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
                    component_id: axial_minecraft::LoaderComponentId::Fabric,
                    version: "0.14.22".to_string(),
                }),
                files: Vec::new(),
            },
        )
        .expect("pack target");

        assert_eq!(target.minecraft, "1.20.1");
        assert_eq!(target.loader.as_deref(), Some("fabric"));
        let build_id = axial_minecraft::build_id_for(
            axial_minecraft::LoaderComponentId::Fabric,
            "1.20.1",
            "0.14.22",
        );
        assert_eq!(
            target.selection_id,
            format!("loader_build|net.fabricmc.fabric-loader|{build_id}")
        );
        assert_eq!(
            axial_minecraft::parse_build_id(&build_id),
            Some((
                axial_minecraft::LoaderComponentId::Fabric,
                "1.20.1".to_string(),
                "0.14.22".to_string(),
            ))
        );
    }

    #[test]
    fn omitted_modpack_version_is_replaced_with_the_resolved_version() {
        let request = pinned_modpack_queue_request(
            ModpackInstallRequest {
                instance_id: "instance-1".to_string(),
                canonical_id: "modrinth:pack".to_string(),
                version_id: None,
                selected_file_ids: Vec::new(),
                include_overrides: true,
            },
            CanonicalId("modrinth:pack".to_string()),
            "resolved-version".to_string(),
            "Pack".to_string(),
        );

        let InstallQueueRequest::Content {
            action: InstallQueueContentActionRequest::Modpack { version_id, .. },
            ..
        } = request
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
            selected_file_ids: vec![modpack_file_selection_id(
                &CanonicalId("modrinth:pack".to_string()),
                "version",
                "mods/example.jar",
            )],
            include_overrides: true,
        };

        assert!(reject_cherry_pick_overrides(&request).is_err());
    }

    #[test]
    fn cherry_pick_validation_rejects_unidentified_incompatible_and_occupied_files() {
        let pack_id = CanonicalId("modrinth:pack".to_string());
        let option = |path: &str,
                      identified: bool,
                      compatible: bool,
                      installed: bool|
         -> ClassifiedModpackFile {
            ClassifiedModpackFile {
                path: path.to_string(),
                option: ModpackFileOption {
                    selection_id: modpack_file_selection_id(&pack_id, "version", path),
                    filename: path.rsplit('/').next().unwrap_or(path).to_string(),
                    kind: ContentKind::Mod,
                    size: None,
                    title: "Example".to_string(),
                    identified,
                    compatible,
                    installed,
                },
            }
        };
        let classified = vec![
            option("mods/good.jar", true, true, false),
            option("mods/unknown.jar", false, false, false),
            option("mods/incompatible.jar", true, false, false),
            option("mods/occupied.jar", true, true, true),
        ];

        let good_id = classified[0].option.selection_id.clone();
        assert_eq!(
            resolve_selected_paths(&[good_id], &classified).expect("eligible selection"),
            vec!["mods/good.jar".to_string()]
        );
        for rejected in classified
            .iter()
            .skip(1)
            .map(|file| &file.option.selection_id)
        {
            assert!(
                resolve_selected_paths(std::slice::from_ref(rejected), &classified).is_err(),
                "{rejected} must be rejected",
            );
        }
        let missing_id = modpack_file_selection_id(&pack_id, "version", "mods/missing.jar");
        assert!(resolve_selected_paths(&[missing_id], &classified).is_err());
    }

    #[test]
    fn modpack_file_selection_ids_are_stable_opaque_and_domain_bound() {
        let pack = CanonicalId("modrinth:pack".to_string());
        let id = modpack_file_selection_id(&pack, "version-1", "mods/example.jar");

        assert!(valid_modpack_file_selection_id(&id));
        assert_eq!(
            id,
            modpack_file_selection_id(&pack, "version-1", "mods/example.jar")
        );
        assert_ne!(
            id,
            modpack_file_selection_id(&pack, "version-2", "mods/example.jar")
        );
        assert_ne!(
            id,
            modpack_file_selection_id(
                &CanonicalId("modrinth:other-pack".to_string()),
                "version-1",
                "mods/example.jar",
            )
        );
        assert_ne!(
            id,
            modpack_file_selection_id(&pack, "version-1", "mods/other.jar")
        );
        assert!(!id.contains("example"));
    }

    #[test]
    fn selected_file_ids_have_strict_shape_count_and_uniqueness_bounds() {
        let valid = modpack_file_selection_id(
            &CanonicalId("modrinth:pack".to_string()),
            "version",
            "mods/example.jar",
        );
        assert!(validate_modpack_file_selection_ids(&[]).is_ok());
        assert!(validate_modpack_file_selection_ids(std::slice::from_ref(&valid)).is_ok());
        assert!(validate_modpack_file_selection_ids(&[valid.clone(), valid]).is_err());
        assert!(validate_modpack_file_selection_ids(&["mods/example.jar".to_string()]).is_err());
        assert!(
            validate_modpack_file_selection_ids(&[format!("mpf1-{}", "A".repeat(64))]).is_err()
        );

        let too_many = (0..=MAX_MODPACK_FILE_SELECTIONS)
            .map(|index| format!("mpf1-{index:064x}"))
            .collect::<Vec<_>>();
        assert!(validate_modpack_file_selection_ids(&too_many).is_err());
    }

    #[test]
    fn oversized_provider_selection_surface_is_typed_metadata_failure() {
        let index = PackIndex {
            name: "Oversized".to_string(),
            version: "1".to_string(),
            minecraft: "1.21.6".to_string(),
            loader: None,
            files: (0..=MAX_MODPACK_FILE_SELECTIONS)
                .map(|index| axial_content::PackFile {
                    path: format!("mods/file-{index}.jar"),
                    url: format!("https://example.invalid/file-{index}.jar"),
                    sha1: None,
                    sha512: Some(format!("{index:0128x}")),
                    size: Some(1),
                })
                .collect(),
        };

        let error = validate_selection_surface(&index)
            .expect_err("provider-authored selection surface must be bounded");
        let ((status, _), failure_kind) = content_execution_error(error).into_parts();

        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert_eq!(
            failure_kind,
            Some(ContentExecutionFailureKind::MetadataInvalid)
        );
    }

    #[test]
    fn public_modpack_file_option_contains_no_archive_path() {
        let option = ModpackFileOption {
            selection_id: modpack_file_selection_id(
                &CanonicalId("modrinth:pack".to_string()),
                "version",
                "mods/private-provider-name.jar",
            ),
            filename: "private-provider-name.jar".to_string(),
            kind: ContentKind::Mod,
            size: Some(42),
            title: "Example".to_string(),
            identified: true,
            compatible: true,
            installed: false,
        };
        let value = serde_json::to_value(option).expect("serialize public option");

        assert!(value.get("selection_id").is_some());
        assert!(value.get("path").is_none());
        assert!(!value.to_string().contains("mods/"));
    }

    #[test]
    fn old_selected_paths_request_field_is_rejected() {
        let request = serde_json::from_value::<ModpackInstallRequest>(serde_json::json!({
            "instance_id": "instance-1",
            "canonical_id": "modrinth:pack",
            "version_id": "version",
            "selected_paths": ["mods/example.jar"],
            "include_overrides": false
        }));

        assert!(request.is_err());
    }

    #[test]
    fn cherry_pick_resolution_requires_the_complete_dependency_closure() {
        let item = |project: &str, reason: axial_content::ResolutionReason, installed: bool| {
            axial_content::ResolvedContentItem {
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
        let resolution = ContentResolution {
            items: vec![
                item("selected", axial_content::ResolutionReason::Selected, false),
                item(
                    "dependency",
                    axial_content::ResolutionReason::Dependency,
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
