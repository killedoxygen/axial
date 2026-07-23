use crate::error::{ContentError, ContentResult};
use crate::limits::{
    MAX_CONTENT_ARTIFACT_BYTES, MAX_CONTENT_GRAPH_BYTES, MAX_DEPENDENCIES_PER_NODE,
    MAX_RESOLUTION_EDGES, MAX_RESOLUTION_NODES,
};
use crate::manifest::{ContentManifest, ManifestEntry, entry_path_matches};
use crate::model::{
    CanonicalId, ContentDependency, ContentKind, FileRef, ManagedContentFileName, ProviderId,
};
use crate::transaction::{FileTransaction, ManagedContentInventory, contained_path};
use axial_minecraft::portable_path::{
    PortableFileName, PortablePathKey, PortableRelativePath, managed_content_name_is_reserved,
};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;
use url::Url;

/// A single resolved file the pipeline should download and record. Callers build
/// these from a resolution plan (selected content plus its dependencies).
#[derive(Clone)]
pub struct PlannedFile {
    pub canonical_id: CanonicalId,
    pub provider: ProviderId,
    pub project_id: String,
    pub version_id: String,
    pub kind: ContentKind,
    file: PlannedArtifact,
    pub dependencies: Vec<ContentDependency>,
    pub title: Option<String>,
}

#[derive(Clone)]
struct PlannedArtifact {
    filename: ManagedContentFileName,
    download_url: Url,
    sha1: Option<String>,
    sha512: String,
    size: u64,
}

impl PlannedFile {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        canonical_id: CanonicalId,
        provider: ProviderId,
        project_id: String,
        version_id: String,
        kind: ContentKind,
        file: FileRef,
        dependencies: Vec<ContentDependency>,
        title: Option<String>,
    ) -> ContentResult<Self> {
        Ok(Self {
            canonical_id,
            provider,
            project_id,
            version_id,
            kind,
            file: PlannedArtifact::admit(kind, &file)?,
            dependencies,
            title,
        })
    }

    pub(crate) fn filename(&self) -> &ManagedContentFileName {
        &self.file.filename
    }

    pub(crate) fn download_url(&self) -> &Url {
        &self.file.download_url
    }

    pub(crate) fn sha1(&self) -> Option<&str> {
        self.file.sha1.as_deref()
    }

    pub(crate) fn sha512(&self) -> &str {
        &self.file.sha512
    }

    pub(crate) fn size(&self) -> u64 {
        self.file.size
    }
}

impl PlannedArtifact {
    fn admit(kind: ContentKind, file: &FileRef) -> ContentResult<Self> {
        let (size, download_url) = validate_planned_artifact(kind, file)?;
        Ok(Self {
            filename: ManagedContentFileName::new_exact(&file.filename).map_err(|_| {
                ContentError::ProviderMetadataInvalid(
                    "the provider returned an invalid content filename".to_string(),
                )
            })?,
            download_url,
            sha1: file.sha1.clone(),
            sha512: file.sha512.clone().ok_or_else(|| {
                ContentError::ProviderMetadataInvalid(
                    "the provider returned content without an exact SHA-512 digest".to_string(),
                )
            })?,
            size,
        })
    }
}

/// Revalidate a resolved plan immediately before any staging or filesystem
/// mutation. Resolution is the primary admission boundary, while this guard
/// prevents another in-process caller from constructing a weaker plan.
pub(crate) fn validate_install_plan(files: &[PlannedFile]) -> ContentResult<()> {
    if files.len() > MAX_RESOLUTION_NODES {
        return Err(ContentError::ProviderMetadataInvalid(
            "the content plan exceeds its item bound".to_string(),
        ));
    }
    let mut edge_count = 0_usize;
    let mut total_bytes = 0_u64;
    for planned in files {
        if planned.dependencies.len() > MAX_DEPENDENCIES_PER_NODE {
            return Err(ContentError::ProviderMetadataInvalid(
                "the content plan exceeds its per-item dependency bound".to_string(),
            ));
        }
        edge_count = edge_count
            .checked_add(planned.dependencies.len())
            .filter(|count| *count <= MAX_RESOLUTION_EDGES)
            .ok_or_else(|| {
                ContentError::ProviderMetadataInvalid(
                    "the content plan exceeds its dependency bound".to_string(),
                )
            })?;
        let artifact_bytes = planned.file.size;
        total_bytes = total_bytes
            .checked_add(artifact_bytes)
            .filter(|bytes| *bytes <= MAX_CONTENT_GRAPH_BYTES)
            .ok_or_else(|| {
                ContentError::ProviderMetadataInvalid(
                    "the content plan exceeds its aggregate download bound".to_string(),
                )
            })?;
    }
    Ok(())
}

pub(crate) fn validate_planned_artifact(
    kind: ContentKind,
    file: &FileRef,
) -> ContentResult<(u64, Url)> {
    if kind == ContentKind::Modpack {
        return Err(ContentError::ProviderMetadataInvalid(
            "a modpack is not installable as a single content artifact".to_string(),
        ));
    }
    let filename = ManagedContentFileName::new_exact(&file.filename).map_err(|_| {
        ContentError::ProviderMetadataInvalid(
            "the provider returned an invalid content filename".to_string(),
        )
    })?;
    if kind == ContentKind::Mod && !filename.key().as_str().ends_with(".jar") {
        return Err(ContentError::ProviderMetadataInvalid(
            "the provider returned an invalid content filename".to_string(),
        ));
    }
    let url = Url::parse(&file.url).map_err(|_| {
        ContentError::ProviderMetadataInvalid(
            "the provider returned an invalid content download URL".to_string(),
        )
    })?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err(ContentError::ProviderMetadataInvalid(
            "content downloads require an HTTPS provider URL".to_string(),
        ));
    }
    if !file.sha512.as_deref().is_some_and(valid_sha512) {
        return Err(ContentError::ProviderMetadataInvalid(
            "the provider returned content without an exact SHA-512 digest".to_string(),
        ));
    }
    let size = file.size.filter(|size| *size > 0).ok_or_else(|| {
        ContentError::ProviderMetadataInvalid(
            "the provider returned content without a positive size".to_string(),
        )
    })?;
    if size > MAX_CONTENT_ARTIFACT_BYTES {
        return Err(ContentError::ProviderMetadataInvalid(
            "the provider returned an oversized content artifact".to_string(),
        ));
    }
    Ok((size, url))
}

fn valid_sha512(value: &str) -> bool {
    value.len() == 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagedRemoval {
    relative: PortableRelativePath,
    owner: ManifestEntry,
    present: bool,
}

/// Prevalidated portable identities protected from stale ownership cleanup.
/// Construction is linear once, while every stale variant lookup is O(1).
#[derive(Debug, Clone, Default)]
pub struct ProtectedManagedPaths {
    keys: HashSet<PortablePathKey>,
}

impl ProtectedManagedPaths {
    pub fn new(relative_paths: &[String]) -> ContentResult<Self> {
        let mut keys = HashSet::with_capacity(relative_paths.len());
        for relative in relative_paths {
            let relative = PortableRelativePath::new_exact(relative).map_err(|_| {
                ContentError::Invalid("protected content path is invalid".to_string())
            })?;
            keys.insert(relative.key());
        }
        Ok(Self { keys })
    }

    fn contains(&self, relative: &PortableRelativePath) -> bool {
        self.keys.contains(&relative.key())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModFileToggleOutcome {
    pub filename: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModFileDeleteOutcome {
    Deleted,
    Managed,
}

#[derive(Debug, thiserror::Error)]
pub enum ModFileMutationError {
    #[error("mod file was not found")]
    NotFound,
    #[error("mod files changed during the operation")]
    Conflict,
    #[error(transparent)]
    Failed(ContentError),
}

impl ManagedRemoval {
    pub fn relative_path(&self) -> &str {
        self.relative.as_str()
    }
}

pub(crate) fn stage_managed_removals(
    transaction: &mut FileTransaction,
    removals: &[ManagedRemoval],
) -> ContentResult<()> {
    let mut owners =
        HashMap::<PortablePathKey, (PortableRelativePath, ManifestEntry, bool)>::new();
    for removal in removals {
        let key = removal.relative.key();
        match owners.get(&key) {
            Some((relative, owner, present))
                if relative == &removal.relative
                    && owner == &removal.owner
                    && *present == removal.present => {}
            Some(_) => {
                return Err(ContentError::Invalid(
                    "multiple manifest owners claim the same removal path".to_string(),
                ));
            }
            None => {
                owners.insert(
                    key,
                    (removal.relative.clone(), removal.owner.clone(), removal.present),
                );
            }
        }
    }
    let mut guarded_paths = owners
        .values()
        .map(|(relative, _, _)| relative.to_string())
        .collect::<Vec<_>>();
    guarded_paths.sort();
    let mut paired_owners = HashSet::new();
    let mut variant_pairs = Vec::new();
    for (_, owner, _) in owners.values() {
        if !paired_owners.insert(owner.canonical_id().clone()) {
            continue;
        }
        let variants = managed_entry_variant_paths(owner)?;
        let [enabled, disabled] = variants.as_slice() else {
            return Err(ContentError::Invalid(
                "managed content removal does not have an enabled and disabled variant"
                    .to_string(),
            ));
        };
        variant_pairs.push((enabled.to_string(), disabled.to_string()));
    }
    transaction.guard_managed_file_variants(&variant_pairs)?;
    let relative_paths = guarded_paths;
    transaction.stage_removals_with_revalidation(&relative_paths, |relative, claimed| {
        let key = PortableRelativePath::new_exact(relative)
            .map(|relative| relative.key())
            .map_err(|_| ContentError::Invalid("managed content path is invalid".to_string()))?;
        let Some((_, owner, present)) = owners.get(&key) else {
            return Err(ContentError::Invalid(
                "content removal has no current ownership proof".to_string(),
            ));
        };
        if !*present {
            return Err(ContentError::Invalid(
                "an absent content removal unexpectedly became present".to_string(),
            ));
        }
        if entry_path_matches(claimed, owner) {
            Ok(())
        } else {
            Err(ContentError::Invalid(
                "a managed content file changed before removal commit".to_string(),
            ))
        }
    })
}

/// Toggle a mod file by claiming the exact source bytes before deciding whether
/// they are still manifest-owned. The target is published without replacement,
/// and a manifest failure rolls the move back without clobbering a path that
/// appeared in the meantime.
pub fn toggle_mod_file(
    game_dir: &Path,
    source_filename: &str,
    enabled: bool,
) -> Result<ModFileToggleOutcome, ModFileMutationError> {
    toggle_mod_file_with_hooks(game_dir, source_filename, enabled, || {}, || {}, || {})
        .map_err(classify_mod_file_mutation_error)
}

fn toggle_mod_file_with_hooks<B, P, S>(
    game_dir: &Path,
    source_filename: &str,
    enabled: bool,
    before_claim: B,
    before_publish: P,
    before_manifest_save: S,
) -> ContentResult<ModFileToggleOutcome>
where
    B: FnOnce(),
    P: FnOnce(),
    S: FnOnce(),
{
    validate_mod_filename(source_filename)?;
    let target_filename = mod_enabled_filename(source_filename, enabled)?;
    let source_relative = format!("mods/{source_filename}");
    let target_relative = format!("mods/{target_filename}");
    let mut manifest = ContentManifest::load(game_dir)?;
    let mut transaction = FileTransaction::empty(game_dir)?;

    before_claim();
    let source_guard = vec![source_relative.clone()];
    let source_inventory = ManagedContentInventory::capture(game_dir, &source_guard)?;
    if !source_inventory.require_exact_or_absent(&source_relative)? {
        return Err(ContentError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "mod file disappeared before it could be claimed",
        )));
    }
    let managed_candidates = manifest_mod_candidates(&manifest, source_filename);
    if source_relative == target_relative {
        let mut claimed = false;
        let mut managed_index = None;
        transaction.stage_removals_with_revalidation(
            std::slice::from_ref(&source_relative),
            |_, claimed_path| {
                require_regular_claimed_mod(claimed_path)?;
                claimed = true;
                managed_index = matching_managed_mod(&manifest, &managed_candidates, claimed_path);
                Ok(())
            },
        )?;
        if !claimed {
            return Err(ContentError::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "mod file disappeared before it could be claimed",
            )));
        }
        before_publish();
        transaction.rollback()?;
        let manifest_changed = if let Some(index) = managed_index {
            let canonical_id = manifest.entries()[index].canonical_id().clone();
            manifest
                .try_set_enabled(&canonical_id, enabled)?
                .unwrap_or(false)
        } else {
            false
        };
        if manifest_changed {
            let guarded_paths = vec![source_relative.clone()];
            let inventory = ManagedContentInventory::capture(game_dir, &guarded_paths)?;
            before_manifest_save();
            manifest.save_with_revalidation(game_dir, || {
                inventory.verify(game_dir, &guarded_paths)
            })?;
        }
        return Ok(ModFileToggleOutcome {
            filename: target_filename,
        });
    }

    let managed_index = transaction.move_new_with_revalidation(
        &source_relative,
        &target_relative,
        |claimed_path| {
            require_regular_claimed_mod(claimed_path)?;
            Ok(matching_managed_mod(
                &manifest,
                &managed_candidates,
                claimed_path,
            ))
        },
        before_publish,
    )?;
    let manifest_changed = if let Some(index) = managed_index {
        let canonical_id = manifest.entries()[index].canonical_id().clone();
        manifest
            .try_set_enabled(&canonical_id, enabled)?
            .unwrap_or(false)
    } else {
        false
    };
    if manifest_changed {
        before_manifest_save();
        if let Err(error) =
            manifest.save_with_revalidation(game_dir, || transaction.verify_managed_inventory())
        {
            return match transaction.rollback() {
                Ok(()) => Err(error),
                Err(rollback_error) => Err(rollback_error),
            };
        }
    }
    if manifest_changed {
        transaction.commit_after_verified_publication();
    } else {
        transaction.commit()?;
    }

    Ok(ModFileToggleOutcome {
        filename: target_filename,
    })
}

/// Delete only after claiming and classifying the exact bytes at the requested
/// path. A still-managed file is restored without replacement and reported to
/// the caller instead of being removed.
pub fn delete_local_mod_file(
    game_dir: &Path,
    source_filename: &str,
) -> Result<ModFileDeleteOutcome, ModFileMutationError> {
    delete_local_mod_file_with_before_claim(game_dir, source_filename, || {})
        .map_err(classify_mod_file_mutation_error)
}

fn classify_mod_file_mutation_error(error: ContentError) -> ModFileMutationError {
    match error {
        ContentError::Io(error) if error.kind() == std::io::ErrorKind::NotFound => {
            ModFileMutationError::NotFound
        }
        ContentError::Invalid(_) => ModFileMutationError::Conflict,
        error => ModFileMutationError::Failed(error),
    }
}

fn delete_local_mod_file_with_before_claim<B>(
    game_dir: &Path,
    source_filename: &str,
    before_claim: B,
) -> ContentResult<ModFileDeleteOutcome>
where
    B: FnOnce(),
{
    validate_mod_filename(source_filename)?;
    let source_relative = format!("mods/{source_filename}");
    let manifest = ContentManifest::load(game_dir)?;
    let mut transaction = FileTransaction::empty(game_dir)?;
    let mut claimed = false;
    let mut managed = false;

    before_claim();
    let source_guard = vec![source_relative.clone()];
    let source_inventory = ManagedContentInventory::capture(game_dir, &source_guard)?;
    if !source_inventory.require_exact_or_absent(&source_relative)? {
        return Err(ContentError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "mod file disappeared before it could be claimed",
        )));
    }
    let managed_candidates = manifest_mod_candidates(&manifest, source_filename);
    transaction.stage_removals_with_revalidation(
        std::slice::from_ref(&source_relative),
        |_, claimed_path| {
            require_regular_claimed_mod(claimed_path)?;
            claimed = true;
            managed = matching_managed_mod(&manifest, &managed_candidates, claimed_path).is_some();
            Ok(())
        },
    )?;
    if !claimed {
        return Err(ContentError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "mod file disappeared before it could be claimed",
        )));
    }
    if managed {
        transaction.rollback()?;
        Ok(ModFileDeleteOutcome::Managed)
    } else {
        transaction.commit()?;
        Ok(ModFileDeleteOutcome::Deleted)
    }
}

fn matching_managed_mod(
    manifest: &ContentManifest,
    candidates: &[usize],
    claimed_path: &Path,
) -> Option<usize> {
    candidates
        .iter()
        .copied()
        .find(|index| entry_path_matches(claimed_path, &manifest.entries()[*index]))
}

fn manifest_mod_candidates(
    manifest: &ContentManifest,
    source_filename: &str,
) -> Vec<usize> {
    manifest
        .entries()
        .iter()
        .enumerate()
        .filter_map(|(index, entry)| {
            (entry.kind() == ContentKind::Mod)
                .then(|| entry.managed_filename())
                .flatten()
                .is_some_and(|filename| {
                    filename.as_str() == source_filename
                        || filename.disabled().as_str() == source_filename
                })
                .then_some(index)
        })
        .collect()
}

fn require_regular_claimed_mod(path: &Path) -> ContentResult<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.is_file() {
        Ok(())
    } else {
        Err(ContentError::Invalid(
            "mod mutation source is not a regular file".to_string(),
        ))
    }
}

fn validate_mod_filename(filename: &str) -> ContentResult<()> {
    let portable = PortableFileName::new_exact(filename)
        .map_err(|_| ContentError::Invalid("mod filename is invalid".to_string()))?;
    let key = portable.key();
    if managed_content_name_is_reserved(&portable)
        || (!key.as_str().ends_with(".jar") && !key.as_str().ends_with(".jar.disabled"))
    {
        return Err(ContentError::Invalid("mod filename is invalid".to_string()));
    }
    Ok(())
}

fn mod_enabled_filename(filename: &str, enabled: bool) -> ContentResult<String> {
    let portable = PortableFileName::new_exact(filename)
        .map_err(|_| ContentError::Invalid("mod filename is invalid".to_string()))?;
    let disabled = portable.key().as_str().ends_with(".disabled");
    if enabled {
        if disabled {
            Ok(filename[..filename.len() - ".disabled".len()].to_string())
        } else {
            Ok(filename.to_string())
        }
    } else if disabled {
        Ok(filename.to_string())
    } else {
        portable
            .with_suffix(".disabled")
            .map(|name| name.to_string())
            .map_err(|_| ContentError::Invalid("mod filename is invalid".to_string()))
    }
}

/// Return every unprotected managed variant with its observed presence. A live
/// path whose bytes no longer match provenance is user-owned and aborts the
/// whole cleanup. Absent variants remain guarded through commit so a late path
/// cannot appear beside the removed ownership record.
pub fn verified_removable_variants(
    game_dir: &Path,
    entry: &ManifestEntry,
    protected_paths: &ProtectedManagedPaths,
) -> ContentResult<Vec<ManagedRemoval>> {
    let mut removable = Vec::new();
    for relative in managed_entry_variant_paths(entry)? {
        if protected_paths.contains(&relative) {
            continue;
        }
        let path = contained_path(game_dir, relative.as_str())?;
        match fs::symlink_metadata(&path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                removable.push(ManagedRemoval {
                    relative,
                    owner: entry.clone(),
                    present: false,
                });
            }
            Err(error) => return Err(ContentError::Io(error)),
            Ok(metadata) if !metadata.is_file() => {
                return Err(ContentError::Invalid(
                    "a managed content path is no longer a regular file".to_string(),
                ));
            }
            Ok(_) if entry_path_matches(&path, entry) => {
                removable.push(ManagedRemoval {
                    relative,
                    owner: entry.clone(),
                    present: true,
                });
            }
            Ok(_) => {
                return Err(ContentError::Invalid(
                    "a managed content file changed outside the launcher".to_string(),
                ));
            }
        }
    }
    Ok(removable)
}

pub(crate) fn managed_variant_paths(
    kind: ContentKind,
    filename: &ManagedContentFileName,
) -> ContentResult<Vec<PortableRelativePath>> {
    let Some(kind_dir) = kind.install_subdir() else {
        return Ok(Vec::new());
    };
    let disabled = filename.disabled();
    [filename.as_str(), disabled.as_str()]
        .into_iter()
        .map(|filename| {
            PortableRelativePath::new_exact(&format!("{kind_dir}/{filename}")).map_err(|_| {
                ContentError::ProviderMetadataInvalid(
                    "the provider returned an invalid content destination".to_string(),
                )
            })
        })
        .collect()
}

pub(crate) fn managed_entry_variant_paths(
    entry: &ManifestEntry,
) -> ContentResult<Vec<PortableRelativePath>> {
    match entry.managed_filename() {
        Some(filename) => managed_variant_paths(entry.kind(), filename).map_err(|_| {
            ContentError::Invalid("managed content path is invalid".to_string())
        }),
        None if entry.kind() == ContentKind::Modpack => Ok(Vec::new()),
        None => Err(ContentError::Invalid(
            "managed content path is invalid".to_string(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::DependencyKind;

    fn planned(project: &str, filename: &str) -> PlannedFile {
        PlannedFile::new(
            CanonicalId::for_project(ProviderId::Modrinth, project),
            ProviderId::Modrinth,
            project.to_string(),
            format!("{project}-version"),
            ContentKind::Mod,
            FileRef {
                url: format!("https://example.invalid/{filename}"),
                filename: filename.to_string(),
                sha1: None,
                sha512: Some("a".repeat(128)),
                size: Some(1),
                primary: true,
            },
            Vec::new(),
            Some(project.to_string()),
        )
        .expect("valid planned content")
    }

    fn dependency(index: usize) -> ContentDependency {
        ContentDependency {
            project_id: Some(format!("dependency-{index}")),
            version_id: None,
            kind: DependencyKind::Required,
        }
    }

    #[test]
    fn artifact_admission_is_exact_and_closed() {
        let mut candidate = FileRef {
            url: "https://example.invalid/project.jar".to_string(),
            filename: "project.jar".to_string(),
            sha1: None,
            sha512: Some("a".repeat(128)),
            size: Some(MAX_CONTENT_ARTIFACT_BYTES),
            primary: true,
        };
        assert_eq!(
            validate_planned_artifact(ContentKind::Mod, &candidate)
                .expect("exact artifact limit")
                .0,
            MAX_CONTENT_ARTIFACT_BYTES
        );

        let mut invalid = candidate.clone();
        invalid.filename = "project.zip".to_string();
        assert!(validate_planned_artifact(ContentKind::Mod, &invalid).is_err());
        invalid = candidate.clone();
        invalid.url = "http://example.invalid/project.jar".to_string();
        assert!(validate_planned_artifact(ContentKind::Mod, &invalid).is_err());
        invalid = candidate.clone();
        invalid.sha512 = Some("A".repeat(128));
        assert!(validate_planned_artifact(ContentKind::Mod, &invalid).is_err());
        invalid = candidate.clone();
        invalid.size = Some(0);
        assert!(validate_planned_artifact(ContentKind::Mod, &invalid).is_err());
        invalid.size = Some(MAX_CONTENT_ARTIFACT_BYTES + 1);
        assert!(validate_planned_artifact(ContentKind::Mod, &invalid).is_err());
    }

    #[test]
    fn install_plan_limits_admit_exact_and_reject_one_over() {
        let exact_nodes = (0..MAX_RESOLUTION_NODES)
            .map(|index| planned(&format!("project-{index}"), &format!("project-{index}.jar")))
            .collect::<Vec<_>>();
        validate_install_plan(&exact_nodes).expect("exact node limit");
        let mut too_many_nodes = exact_nodes;
        too_many_nodes.push(planned("overflow", "overflow.jar"));
        assert!(validate_install_plan(&too_many_nodes).is_err());

        let mut exact_edges = (0..(MAX_RESOLUTION_EDGES / MAX_DEPENDENCIES_PER_NODE))
            .map(|index| planned(&format!("root-{index}"), &format!("root-{index}.jar")))
            .collect::<Vec<_>>();
        for item in &mut exact_edges {
            item.dependencies = (0..MAX_DEPENDENCIES_PER_NODE).map(dependency).collect();
        }
        validate_install_plan(&exact_edges).expect("exact edge limit");
        let mut per_node_over = exact_edges.clone();
        per_node_over[0]
            .dependencies
            .push(dependency(MAX_DEPENDENCIES_PER_NODE));
        assert!(validate_install_plan(&per_node_over).is_err());
        let mut edge_over = exact_edges;
        let mut overflow_edge = planned("edge-over", "edge-over.jar");
        overflow_edge.dependencies.push(dependency(0));
        edge_over.push(overflow_edge);
        assert!(validate_install_plan(&edge_over).is_err());

        let mut exact_graph = planned("large", "large.jar");
        exact_graph.file.size = MAX_CONTENT_GRAPH_BYTES;
        validate_install_plan(std::slice::from_ref(&exact_graph)).expect("exact graph byte limit");
        let mut graph_over = exact_graph;
        graph_over.file.filename = ManagedContentFileName::new_exact("first.jar").unwrap();
        let second = planned("second", "second.jar");
        assert!(validate_install_plan(&[graph_over, second]).is_err());
    }

    fn recorded(project: &str, filename: &str) -> ManifestEntry {
        recorded_with_dependencies(project, filename, Vec::new())
    }

    fn recorded_with_dependencies(
        project: &str,
        filename: &str,
        dependencies: Vec<ContentDependency>,
    ) -> ManifestEntry {
        let planned = planned(project, filename);
        ManifestEntry::managed_file(
            planned.canonical_id,
            planned.provider,
            planned.project_id,
            planned.version_id,
            planned.kind,
            planned.file.filename,
            Some(planned.file.sha512),
            Some(planned.file.size),
            dependencies,
            planned.title,
        )
        .expect("valid recorded content")
    }

    fn insert(
        manifest: &mut ContentManifest,
        entry: ManifestEntry,
    ) -> Option<ManifestEntry> {
        manifest.try_upsert(entry).expect("insert manifest entry")
    }

    fn save_managed_mod(
        root: &Path,
        project: &str,
        filename: &str,
        enabled: bool,
        bytes: &[u8],
    ) -> ContentManifest {
        fs::create_dir_all(root.join("mods")).expect("mods");
        let disk_name = if enabled {
            filename.to_string()
        } else {
            format!("{filename}.disabled")
        };
        let path = root.join("mods").join(disk_name);
        fs::write(&path, bytes).expect("managed mod");
        let mut entry = recorded(project, filename);
        entry.set_enabled(enabled);
        entry
            .record_authenticated_file(
                bytes.len() as u64,
                crate::manifest::sha512_file(&path).expect("managed hash"),
            )
            .expect("record managed file");
        let mut manifest = ContentManifest::default();
        insert(&mut manifest, entry);
        manifest.save(root).expect("save manifest");
        manifest
    }

    fn test_root(name: &str) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!(
            "axial-content-install-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock")
                .as_nanos()
        ));
        fs::create_dir_all(&root).expect("root");
        root
    }

    #[test]
    fn mod_toggle_classifies_the_claimed_replacement_instead_of_preflight_bytes() {
        let root = test_root("mod-toggle-source-replacement");
        let manifest = save_managed_mod(&root, "project", "managed.jar", true, b"managed bytes");
        let manifest_path = crate::manifest::manifest_path(&root);
        let manifest_before = fs::read(&manifest_path).expect("manifest before");
        let source = root.join("mods/managed.jar");
        let target = root.join("mods/managed.jar.disabled");

        let outcome = toggle_mod_file_with_hooks(
            &root,
            "managed.jar",
            false,
            || fs::write(&source, b"user replacement").expect("replace before claim"),
            || {},
            || {},
        )
        .expect("toggle claimed replacement");

        assert_eq!(outcome.filename, "managed.jar.disabled");
        assert!(!source.exists());
        assert_eq!(
            fs::read(&target).expect("moved replacement"),
            b"user replacement"
        );
        assert_eq!(
            fs::read(&manifest_path).expect("manifest after"),
            manifest_before
        );
        assert_eq!(
            ContentManifest::load(&root).expect("load manifest"),
            manifest
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn mod_toggle_preserves_a_target_that_appears_before_publish() {
        let root = test_root("mod-toggle-target-race");
        let manifest = save_managed_mod(&root, "project", "managed.jar", true, b"managed bytes");
        let source = root.join("mods/managed.jar");
        let target = root.join("mods/managed.jar.disabled");

        let error = toggle_mod_file_with_hooks(
            &root,
            "managed.jar",
            false,
            || {},
            || fs::write(&target, b"user target").expect("racing target"),
            || {},
        )
        .expect_err("occupied target must abort");

        assert!(matches!(error, ContentError::Invalid(_)));
        assert_eq!(
            fs::read(&source).expect("restored source"),
            b"managed bytes"
        );
        assert_eq!(fs::read(&target).expect("preserved target"), b"user target");
        assert_eq!(
            ContentManifest::load(&root).expect("load manifest"),
            manifest
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn mod_toggle_rollback_preserves_a_new_source_and_retains_recovery_bytes() {
        let root = test_root("mod-toggle-rollback-source-race");
        save_managed_mod(&root, "project", "managed.jar", true, b"managed bytes");
        let source = root.join("mods/managed.jar");
        let target = root.join("mods/managed.jar.disabled");
        let manifest_path = crate::manifest::manifest_path(&root);
        let external_manifest =
            serde_json::to_vec_pretty(&ContentManifest::default()).expect("external manifest");

        let error = toggle_mod_file_with_hooks(
            &root,
            "managed.jar",
            false,
            || {},
            || {},
            || {
                fs::write(&source, b"user source").expect("racing source");
                fs::write(&manifest_path, &external_manifest).expect("external manifest");
            },
        )
        .expect_err("stale manifest must roll back without clobber");

        assert!(matches!(error, ContentError::Invalid(_)));
        assert_eq!(fs::read(&source).expect("preserved source"), b"user source");
        assert!(!target.exists());
        let recovery = fs::read_dir(&root)
            .expect("root entries")
            .filter_map(Result::ok)
            .map(|entry| entry.path().join(".backup/mods/managed.jar"))
            .find(|path| path.is_file())
            .expect("retained recovery bytes");
        assert_eq!(
            fs::read(recovery).expect("recovery source"),
            b"managed bytes"
        );
        assert!(
            ContentManifest::load(&root)
                .expect("load external manifest")
                .is_empty()
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn mod_delete_classifies_the_claimed_replacement_as_local() {
        let root = test_root("mod-delete-source-replacement");
        let manifest = save_managed_mod(&root, "project", "managed.jar", true, b"managed bytes");
        let manifest_path = crate::manifest::manifest_path(&root);
        let manifest_before = fs::read(&manifest_path).expect("manifest before");
        let source = root.join("mods/managed.jar");

        let outcome = delete_local_mod_file_with_before_claim(&root, "managed.jar", || {
            fs::write(&source, b"user replacement").expect("replace before claim");
        })
        .expect("delete claimed local replacement");

        assert_eq!(outcome, ModFileDeleteOutcome::Deleted);
        assert!(!source.exists());
        assert_eq!(
            fs::read(&manifest_path).expect("manifest after"),
            manifest_before
        );
        assert_eq!(
            ContentManifest::load(&root).expect("load manifest"),
            manifest
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn mod_delete_restores_a_still_managed_claim() {
        let root = test_root("mod-delete-managed");
        let manifest = save_managed_mod(&root, "project", "managed.jar", true, b"managed bytes");
        let source = root.join("mods/managed.jar");

        let outcome =
            delete_local_mod_file(&root, "managed.jar").expect("classify managed deletion");

        assert_eq!(outcome, ModFileDeleteOutcome::Managed);
        assert_eq!(
            fs::read(&source).expect("restored managed source"),
            b"managed bytes"
        );
        assert_eq!(
            ContentManifest::load(&root).expect("load manifest"),
            manifest
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn portable_alias_request_is_rejected_without_claiming_the_exact_entry() {
        let root = test_root("portable-alias");
        save_managed_mod(
            &root,
            "portable-alias",
            "Stra\u{df}e.jar",
            true,
            b"managed",
        );

        assert!(matches!(
            delete_local_mod_file(&root, "STRASSE.JAR"),
            Err(ModFileMutationError::Conflict)
        ));
        assert_eq!(
            fs::read(root.join("mods/Stra\u{df}e.jar")).expect("exact file retained"),
            b"managed"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn distinct_case_sensitive_aliases_fail_closed_without_claiming_either() {
        let root = test_root("case-sensitive-distinct");
        save_managed_mod(&root, "case-alias", "managed.jar", true, b"managed");
        fs::write(root.join("mods/MANAGED.jar"), b"managed").expect("manual");

        assert!(matches!(
            delete_local_mod_file(&root, "MANAGED.jar"),
            Err(ModFileMutationError::Conflict)
        ));
        assert_eq!(
            fs::read(root.join("mods/managed.jar")).expect("managed file retained"),
            b"managed"
        );
        assert_eq!(
            fs::read(root.join("mods/MANAGED.jar")).expect("alias retained"),
            b"managed"
        );
        let _ = fs::remove_dir_all(root);
    }

}
