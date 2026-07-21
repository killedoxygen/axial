use crate::error::{ContentError, ContentResult};
use crate::limits::{
    MAX_CONTENT_ARTIFACT_BYTES, MAX_CONTENT_GRAPH_BYTES, MAX_DEPENDENCIES_PER_NODE,
    MAX_RESOLUTION_EDGES, MAX_RESOLUTION_NODES,
};
use crate::manifest::{ContentManifest, ManifestEntry, entry_file_present, entry_path_matches};
use crate::model::{
    CanonicalId, ContentDependency, ContentKind, FileRef, ManagedContentFileName, ProviderId,
};
use crate::transaction::{
    FileTransaction, ManagedContentInventory, StagingGuard, contained_path,
};
use axial_minecraft::download::{
    DownloadProgress, ExecutionDownloadFact, VerifiedContentIntegrity,
    download_verified_content_to_staging,
};
use axial_minecraft::portable_path::{
    PortableFileName, PortablePathKey, PortableRelativePath, managed_content_name_is_reserved,
};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;
use url::Url;

/// A single resolved file the pipeline should download and record. Callers build
/// these from a resolution plan (selected content plus its dependencies).
#[derive(Debug, Clone)]
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

#[derive(Debug, Clone)]
struct PlannedArtifact {
    filename: ManagedContentFileName,
    download_url: String,
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
}

impl PlannedArtifact {
    fn admit(kind: ContentKind, file: &FileRef) -> ContentResult<Self> {
        let size = validate_planned_artifact(kind, file)?;
        Ok(Self {
            filename: ManagedContentFileName::new_exact(&file.filename).map_err(|_| {
                ContentError::ProviderMetadataInvalid(
                    "the provider returned an invalid content filename".to_string(),
                )
            })?,
            download_url: file.url.clone(),
            sha1: file.sha1.clone(),
            sha512: file
                .sha512
                .clone()
                .expect("validated planned artifacts have an exact SHA-512"),
            size,
        })
    }
}

/// Download and verify each planned file into its instance subdirectory, then
/// record it in the instance manifest. Files land through the shared verified
/// content staging primitive; a replaced file is removed only after its
/// replacement is in place. The manifest is saved once at the end.
pub async fn install_and_record<F, G>(
    client: &reqwest::Client,
    game_dir: &Path,
    files: &[PlannedFile],
    mut on_progress: F,
    mut on_download_fact: G,
) -> ContentResult<ContentManifest>
where
    F: FnMut(DownloadProgress),
    G: FnMut(ExecutionDownloadFact),
{
    validate_install_plan(files)?;
    let mut manifest = ContentManifest::load(game_dir)?;
    let total = files.len() as i32;
    let mut prospective_manifest = manifest.clone();
    let mut entries = files
        .iter()
        .map(|planned| {
            let mut entry = ManifestEntry::managed_file(
                planned.canonical_id.clone(),
                planned.provider,
                planned.project_id.clone(),
                planned.version_id.clone(),
                planned.kind,
                planned.file.filename.clone(),
                Some(planned.file.sha512.clone()),
                Some(planned.file.size),
                planned.dependencies.clone(),
                planned.title.clone(),
            )?;
            // `false` is the longer serialized form, so this projection is an
            // upper bound before any provider-authored path reaches the filesystem.
            entry.set_enabled(false);
            prospective_manifest.validate_provider_entry(&entry)?;
            Ok(entry)
        })
        .collect::<ContentResult<Vec<_>>>()?;
    prospective_manifest
        .try_upsert_batch(entries.clone())
        .map_err(|_| {
            ContentError::ProviderMetadataInvalid(
                "content metadata conflicts in the managed manifest".to_string(),
            )
        })?;
    prospective_manifest.validate_provider_projection()?;
    let install_plan = prepare_install_destinations(game_dir, &manifest, files)?;
    let destinations = &install_plan.destinations;
    for (entry, destination) in entries.iter_mut().zip(destinations) {
        entry.set_enabled(destination.enabled);
    }
    let staging = StagingGuard::create(game_dir, "axial-content-stage")?;

    for (index, (planned, planned_destination)) in files.iter().zip(&destinations).enumerate() {
        let destination = contained_path(staging.path(), planned_destination.relative.as_str())?;
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)?;
        }

        on_progress(progress(
            "download",
            index as i32,
            total,
            Some(planned.file.filename.to_string()),
        ));

        let expected = VerifiedContentIntegrity {
            size: Some(planned.file.size),
            sha1: planned.file.sha1.clone(),
            sha512: Some(planned.file.sha512.clone()),
        };
        match download_verified_content_to_staging(
            client,
            &planned.file.download_url,
            &destination,
            &expected,
        )
            .await
        {
            Ok(report) => {
                entries[index].record_authenticated_file(
                    report.bytes_written,
                    planned.file.sha512.clone(),
                )?;
                for fact in report.facts {
                    on_download_fact(fact);
                }
            }
            Err(error) => {
                for fact in &error.facts {
                    on_download_fact(fact.clone());
                }
                return Err(ContentError::Download(error));
            }
        }
    }

    on_progress(progress("commit", total, total, None));
    let relative_paths: Vec<String> = destinations
        .iter()
        .map(|destination| destination.relative.to_string())
        .collect();
    let mut transaction = apply_install_transaction(game_dir, staging.transfer(), &install_plan)?;
    let stale_entries = manifest.try_upsert_batch(entries)?;

    let protected_paths = ProtectedManagedPaths::new(&relative_paths)?;
    let mut stale_removals = Vec::new();
    for entry in &stale_entries {
        stale_removals.extend(verified_removable_variants(
            game_dir,
            entry,
            &protected_paths,
        )?);
    }
    stage_managed_removals(&mut transaction, &stale_removals)?;
    if let Err(error) =
        manifest.save_with_revalidation(game_dir, || transaction.verify_managed_inventory())
    {
        return match transaction.rollback() {
            Ok(()) => Err(error),
            Err(rollback_error) => Err(rollback_error),
        };
    }
    transaction.commit_after_verified_publication();
    on_progress(done(total));
    Ok(manifest)
}

/// Revalidate a resolved plan immediately before any staging or filesystem
/// mutation. Resolution is the primary admission boundary, while this guard
/// prevents another in-process caller from constructing a weaker plan.
fn validate_install_plan(files: &[PlannedFile]) -> ContentResult<()> {
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

pub(crate) fn validate_planned_artifact(kind: ContentKind, file: &FileRef) -> ContentResult<u64> {
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
    Ok(size)
}

fn valid_sha512(value: &str) -> bool {
    value.len() == 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[derive(Debug)]
struct InstallDestination {
    enabled: bool,
    relative: PortableRelativePath,
    variants: Vec<PortableRelativePath>,
    must_be_absent: bool,
    existing_owner: Option<ManifestEntry>,
}

#[derive(Debug)]
struct InstallPlan {
    destinations: Vec<InstallDestination>,
    guarded_paths: Vec<String>,
    inventory: ManagedContentInventory,
}

/// Resolve and validate every final path before downloading. A manifest entry
/// owns both its enabled and disabled variants; an existing unmanaged path is
/// never implicitly adopted or overwritten.
fn prepare_install_destinations(
    game_dir: &Path,
    manifest: &ContentManifest,
    files: &[PlannedFile],
) -> ContentResult<InstallPlan> {
    let mut seen_ids = HashSet::new();
    let mut batch_variants = HashSet::new();
    let mut variants_by_id: HashMap<CanonicalId, Vec<PortablePathKey>> = HashMap::new();
    let mut manifest_variant_owners: HashMap<PortablePathKey, Vec<&ManifestEntry>> = HashMap::new();
    for entry in manifest.entries() {
        for variant in managed_entry_variant_paths(entry)? {
            manifest_variant_owners
                .entry(variant.key())
                .or_default()
                .push(entry);
        }
    }
    let mut destinations = Vec::with_capacity(files.len());

    for planned in files {
        if !seen_ids.insert(planned.canonical_id.clone()) {
            return Err(ContentError::ProviderMetadataInvalid(
                "the content plan contains the same project more than once".to_string(),
            ));
        }
        let variants = managed_variant_paths(planned.kind, &planned.file.filename)?;
        if variants.len() != 2 {
            return Err(ContentError::ProviderMetadataInvalid(format!(
                "{} is not installable as a single file",
                planned.kind.as_str()
            )));
        }
        for variant in &variants {
            contained_path(game_dir, variant.as_str())?;
            if !batch_variants.insert(variant.key()) {
                return Err(ContentError::ProviderMetadataInvalid(
                    "multiple content projects resolve to the same destination".to_string(),
                ));
            }
        }
        let enabled = preserved_enabled_state(game_dir, manifest, &planned.canonical_id);
        let relative = variants[usize::from(!enabled)].clone();
        variants_by_id.insert(
            planned.canonical_id.clone(),
            variants
                .iter()
                .map(PortableRelativePath::key)
                .collect(),
        );
        destinations.push(InstallDestination {
            enabled,
            relative,
            variants,
            must_be_absent: true,
            existing_owner: None,
        });
    }

    for (planned, destination) in files.iter().zip(&mut destinations) {
        let mut selected_destination_exists = false;
        let mut selected_destination_owner = None;
        for variant in &destination.variants {
            let variant_identity = variant.key();
            let owners = manifest_variant_owners
                .get(&variant_identity)
                .map(Vec::as_slice)
                .unwrap_or_default();
            for owner in owners {
                if owner.canonical_id() == &planned.canonical_id {
                    continue;
                }
                let owner_moves_away = variants_by_id
                    .get(owner.canonical_id())
                    .is_some_and(|new_variants| !new_variants.contains(&variant_identity));
                if !owner_moves_away {
                    return Err(ContentError::Invalid(
                        "a content destination is already owned by another project".to_string(),
                    ));
                }
            }

            let path = contained_path(game_dir, variant.as_str())?;
            match fs::symlink_metadata(&path) {
                Ok(metadata) if metadata.is_file() && !owners.is_empty() => {
                    if owners.iter().any(|owner| !entry_path_matches(&path, owner)) {
                        return Err(ContentError::Invalid(
                            "a content destination is occupied by a file no longer owned by the manifest"
                                .to_string(),
                        ));
                    }
                    if variant == &destination.relative {
                        selected_destination_exists = true;
                        selected_destination_owner = owners.first().map(|owner| (*owner).clone());
                    }
                }
                Ok(_) if owners.is_empty() => {
                    return Err(ContentError::Invalid(
                        "a content destination is already occupied by an unmanaged file"
                            .to_string(),
                    ));
                }
                Ok(_) => {
                    return Err(ContentError::Invalid(
                        "a managed content destination is not a regular file".to_string(),
                    ));
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(ContentError::Io(error)),
            }
        }
        destination.must_be_absent = !selected_destination_exists;
        destination.existing_owner = selected_destination_owner;
    }

    let mut guarded_paths = destinations
        .iter()
        .map(|destination| destination.relative.to_string())
        .collect::<Vec<_>>();
    for planned in files {
        if let Some(existing) = manifest.find(&planned.canonical_id) {
            guarded_paths.extend(
                managed_entry_variant_paths(existing)?
                    .into_iter()
                    .map(|path| path.to_string()),
            );
        }
    }
    guarded_paths.sort();
    guarded_paths.dedup();
    let inventory = ManagedContentInventory::capture(game_dir, &guarded_paths)?;
    for destination in &destinations {
        inventory.require_exact_or_absent(destination.relative.as_str())?;
    }

    Ok(InstallPlan {
        destinations,
        guarded_paths,
        inventory,
    })
}

fn apply_install_transaction(
    game_dir: &Path,
    staging: std::path::PathBuf,
    plan: &InstallPlan,
) -> ContentResult<FileTransaction> {
    let destinations = &plan.destinations;
    let relative_paths = destinations
        .iter()
        .map(|destination| destination.relative.to_string())
        .collect::<Vec<_>>();
    let must_be_absent = destinations
        .iter()
        .filter(|destination| destination.must_be_absent)
        .map(|destination| destination.relative.to_string())
        .collect::<Vec<_>>();
    let replacement_owners = destinations
        .iter()
        .filter_map(|destination| {
            destination
                .existing_owner
                .clone()
                .map(|owner| (destination.relative.to_string(), owner))
        })
        .collect::<HashMap<_, _>>();

    FileTransaction::apply_preserving_absence_with_inventory(
        game_dir,
        staging,
        &relative_paths,
        &plan.guarded_paths,
        plan.inventory.clone(),
        &must_be_absent,
        |relative, destination| {
            let Some(owner) = replacement_owners.get(relative) else {
                return Err(ContentError::Invalid(
                    "content destination has no current ownership proof".to_string(),
                ));
            };
            if entry_path_matches(destination, owner) {
                Ok(())
            } else {
                Err(ContentError::Invalid(
                    "a managed content destination changed before commit".to_string(),
                ))
            }
        },
    )
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

/// Remove a managed file (enabled or disabled variant) and drop its manifest
/// entry. Saves the manifest when an entry was actually removed.
pub fn uninstall(game_dir: &Path, canonical_id: &CanonicalId) -> ContentResult<bool> {
    uninstall_many(game_dir, std::slice::from_ref(canonical_id)).map(|removed| removed > 0)
}

/// Remove a set of managed files in one transaction. Dependencies within the
/// selected set may be removed together, while live content outside the set
/// still protects anything it requires.
pub fn uninstall_many(game_dir: &Path, canonical_ids: &[CanonicalId]) -> ContentResult<usize> {
    let mut manifest = ContentManifest::load(game_dir)?;
    let requested = canonical_ids.iter().collect::<HashSet<_>>();
    let entries = manifest
        .entries()
        .iter()
        .filter(|entry| requested.contains(entry.canonical_id()))
        .cloned()
        .collect::<Vec<_>>();
    if entries.is_empty() {
        return Ok(0);
    }
    let selected = entries
        .iter()
        .map(ManifestEntry::canonical_id)
        .collect::<HashSet<_>>();
    if manifest.entries().iter().any(|dependent| {
        !selected.contains(dependent.canonical_id())
            && entry_file_present(game_dir, dependent)
            && entries.iter().any(|entry| {
                dependent.dependencies().iter().any(|dependency| {
                    dependency.requires_project(entry.project_id(), entry.version_id())
                })
            })
    }) {
        return Err(ContentError::Invalid(
            "content is required by another installed item".to_string(),
        ));
    }
    let mut removable = Vec::new();
    for entry in &entries {
        removable.extend(verified_removable_variants(
            game_dir,
            entry,
            &ProtectedManagedPaths::default(),
        )?);
    }
    for entry in &entries {
        manifest.remove(entry.canonical_id());
    }
    let mut transaction = FileTransaction::empty(game_dir)?;
    stage_managed_removals(&mut transaction, &removable)?;
    if let Err(error) =
        manifest.save_with_revalidation(game_dir, || transaction.verify_managed_inventory())
    {
        return match transaction.rollback() {
            Ok(()) => Err(error),
            Err(rollback_error) => Err(rollback_error),
        };
    }
    transaction.commit_after_verified_publication();
    Ok(entries.len())
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

fn managed_variant_paths(
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

fn managed_entry_variant_paths(entry: &ManifestEntry) -> ContentResult<Vec<PortableRelativePath>> {
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

fn preserved_enabled_state(
    game_dir: &Path,
    manifest: &ContentManifest,
    canonical_id: &CanonicalId,
) -> bool {
    let Some(existing) = manifest.find(canonical_id) else {
        return true;
    };
    let Some(kind_dir) = existing.kind().install_subdir() else {
        return existing.enabled();
    };
    let filename = existing
        .managed_filename()
        .expect("validated file-owning entries have a managed filename");
    let subdir = game_dir.join(kind_dir);
    let enabled_path = subdir.join(filename.as_str());
    let disabled_path = subdir.join(filename.disabled().as_str());
    if enabled_path.exists() {
        true
    } else if disabled_path.exists() {
        false
    } else {
        existing.enabled()
    }
}

fn progress(phase: &str, current: i32, total: i32, file: Option<String>) -> DownloadProgress {
    DownloadProgress {
        phase: phase.to_string(),
        current,
        total,
        file,
        error: None,
        done: false,
        bytes_done: None,
        bytes_total: None,
    }
}

fn done(total: i32) -> DownloadProgress {
    DownloadProgress {
        phase: "done".to_string(),
        current: total,
        total,
        file: None,
        error: None,
        done: true,
        bytes_done: None,
        bytes_total: None,
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
                .expect("exact artifact limit"),
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
    fn unmanaged_enabled_or_disabled_destinations_are_rejected() {
        for alias in [
            "shared.jar",
            "shared.jar.disabled",
            "SHARED.JAR",
            "shared.jar.disabled.disabled",
        ] {
            let root = test_root(if alias == "shared.jar" {
                "unmanaged-enabled"
            } else {
                "unmanaged-disabled"
            });
            fs::create_dir_all(root.join("mods")).expect("mods");
            fs::write(root.join("mods").join(alias), b"user file")
                .expect("unmanaged file");

            assert!(
                prepare_install_destinations(
                    &root,
                    &ContentManifest::default(),
                    &[planned("new", "shared.jar")],
                )
                .is_err(),
                "install destination accepted alias {alias}"
            );
            let _ = fs::remove_dir_all(root);
        }
    }

    #[test]
    fn same_name_manual_replacement_is_not_treated_as_managed() {
        let root = test_root("same-name-replacement");
        fs::create_dir_all(root.join("mods")).expect("mods");
        let path = root.join("mods/shared.jar");
        fs::write(&path, b"launcher-owned bytes").expect("managed file");

        let mut entry = recorded("project", "shared.jar");
        entry
            .record_authenticated_file(
                b"launcher-owned bytes".len() as u64,
                crate::manifest::sha512_file(&path).expect("managed hash"),
            )
            .expect("record managed file");
        let mut manifest = ContentManifest::default();
        insert(&mut manifest, entry);

        fs::write(&path, b"manually replaced user bytes").expect("user replacement");
        let result =
            prepare_install_destinations(&root, &manifest, &[planned("project", "shared.jar")]);

        assert!(result.is_err());
        assert_eq!(
            fs::read(&path).expect("preserved user replacement"),
            b"manually replaced user bytes"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn install_commit_preserves_a_destination_replaced_after_planning() {
        let root = test_root("replacement-after-planning");
        fs::create_dir_all(root.join("mods")).expect("mods");
        let path = root.join("mods/shared.jar");
        fs::write(&path, b"launcher-owned bytes").expect("managed file");

        let mut entry = recorded("project", "shared.jar");
        entry
            .record_authenticated_file(
                b"launcher-owned bytes".len() as u64,
                crate::manifest::sha512_file(&path).expect("managed hash"),
            )
            .expect("record managed file");
        let mut manifest = ContentManifest::default();
        insert(&mut manifest, entry);
        manifest.save(&root).expect("save manifest");

        let planned = planned("project", "shared.jar");
        let destinations =
            prepare_install_destinations(&root, &manifest, std::slice::from_ref(&planned))
                .expect("plan managed replacement");
        let staging = StagingGuard::create(&root, "replacement-after-planning").expect("staging");
        let staged = contained_path(staging.path(), &destinations.destinations[0].relative)
            .expect("contained staged destination");
        fs::create_dir_all(staged.parent().expect("staged parent")).expect("staged parent");
        fs::write(&staged, b"downloaded update").expect("staged update");

        fs::write(&path, b"user replacement").expect("replace after planning");
        let error = match apply_install_transaction(&root, staging.transfer(), &destinations) {
            Ok(transaction) => {
                transaction.rollback().expect("rollback unexpected apply");
                panic!("commit-time ownership revalidation must reject the replacement");
            }
            Err(error) => error,
        };

        assert!(error.to_string().contains("changed before commit"));
        assert_eq!(
            fs::read(&path).expect("preserved user replacement"),
            b"user replacement"
        );
        assert_eq!(
            ContentManifest::load(&root).expect("reload manifest"),
            manifest
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn destination_owned_by_unselected_content_is_rejected() {
        let root = test_root("owned-collision");
        let mut manifest = ContentManifest::default();
        insert(&mut manifest, recorded("existing", "shared.jar"));

        assert!(
            prepare_install_destinations(&root, &manifest, &[planned("new", "shared.jar")],)
                .is_err()
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn batch_can_reuse_a_path_only_when_its_owner_moves_away() {
        let root = test_root("owned-batch-move");
        let mut manifest = ContentManifest::default();
        insert(&mut manifest, recorded("first", "common.jar"));
        insert(&mut manifest, recorded("second", "second.jar"));

        let result = prepare_install_destinations(
            &root,
            &manifest,
            &[
                planned("first", "first.jar"),
                planned("second", "common.jar"),
            ],
        );

        assert!(result.is_ok());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn batch_projects_cannot_claim_the_same_destination() {
        let root = test_root("batch-collision");

        assert!(
            prepare_install_destinations(
                &root,
                &ContentManifest::default(),
                &[
                    planned("first", "shared.jar"),
                    planned("second", "shared.jar")
                ],
            )
            .is_err()
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn invalid_provider_single_file_metadata_is_typed() {
        let root = test_root("provider-metadata");
        let duplicate_destination = prepare_install_destinations(
            &root,
            &ContentManifest::default(),
            &[
                planned("first", "shared.jar"),
                planned("second", "shared.jar"),
            ],
        )
        .expect_err("provider destination collision must fail");
        assert!(matches!(
            duplicate_destination,
            ContentError::ProviderMetadataInvalid(_)
        ));

        let invalid_filename = prepare_install_destinations(
            &root,
            &ContentManifest::default(),
            &[planned("first", "../shared.jar")],
        )
        .expect_err("provider path must fail");
        assert!(matches!(
            invalid_filename,
            ContentError::ProviderMetadataInvalid(_)
        ));

        let overlong_filename = prepare_install_destinations(
            &root,
            &ContentManifest::default(),
            &[planned("first", &format!("{}.jar", "x".repeat(252)))],
        )
        .expect_err("provider filename bound must fail before filesystem inspection");
        assert!(matches!(
            overlong_filename,
            ContentError::ProviderMetadataInvalid(_)
        ));

        let case_folded_collision = prepare_install_destinations(
            &root,
            &ContentManifest::default(),
            &[
                planned("first", "shared.jar"),
                planned("second", "SHARED.jar"),
            ],
        )
        .expect_err("portable ownership identity must reject case-folded collisions");
        assert!(matches!(
            case_folded_collision,
            ContentError::ProviderMetadataInvalid(_)
        ));

        let unicode_folded_collision = prepare_install_destinations(
            &root,
            &ContentManifest::default(),
            &[
                planned("unicode-first", "Stra\u{df}e.jar"),
                planned("unicode-second", "STRASSE.jar"),
            ],
        )
        .expect_err("full Unicode folding must reject destination aliases");
        assert!(matches!(
            unicode_folded_collision,
            ContentError::ProviderMetadataInvalid(_)
        ));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn kind_change_displaces_the_exact_old_ownership_path_for_cleanup() {
        let root = test_root("kind-change-cleanup");
        fs::create_dir_all(root.join("mods")).expect("mods");
        let old_path = root.join("mods/shared.jar");
        fs::write(&old_path, b"managed bytes").expect("managed");
        let mut old = recorded("project", "shared.jar");
        old.record_authenticated_file(
            b"managed bytes".len() as u64,
            crate::manifest::sha512_file(&old_path).expect("managed hash"),
        )
        .expect("record managed file");
        let mut manifest = ContentManifest::default();
        insert(&mut manifest, old.clone());

        let replacement = ManifestEntry::managed_file(
            old.canonical_id().clone(),
            old.provider(),
            old.project_id().to_string(),
            old.version_id().to_string(),
            ContentKind::ResourcePack,
            old.managed_filename().expect("managed filename").clone(),
            old.sha512().map(str::to_string),
            old.size(),
            old.dependencies().to_vec(),
            old.title().map(str::to_string),
        )
        .expect("valid replacement");
        let displaced = manifest
            .try_upsert(replacement)
            .expect("replace entry")
            .expect("kind change must displace old ownership");
        let removals = verified_removable_variants(
            &root,
            &displaced,
            &ProtectedManagedPaths::default(),
        )
        .expect("old ownership cleanup");
        let mut transaction = FileTransaction::empty(&root).expect("transaction");
        stage_managed_removals(&mut transaction, &removals).expect("stage old path");
        transaction.commit().expect("verified commit");

        assert!(!old_path.exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn updates_preserve_the_existing_disabled_file_state() {
        let root = std::env::temp_dir().join(format!(
            "axial-content-disabled-update-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock")
                .as_nanos()
        ));
        let mods = root.join("mods");
        fs::create_dir_all(&mods).expect("create mods directory");
        fs::write(mods.join("old.jar.disabled"), b"disabled").expect("write disabled mod");
        let id = CanonicalId::for_project(ProviderId::Modrinth, "project");
        let mut manifest = ContentManifest::default();
        insert(&mut manifest, ManifestEntry::managed(
            id.clone(),
            ProviderId::Modrinth,
            "project".to_string(),
            "old-version".to_string(),
            ContentKind::Mod,
            &FileRef {
                url: "https://example.invalid/old.jar".to_string(),
                filename: "old.jar".to_string(),
                sha1: None,
                sha512: Some(
                    crate::manifest::sha512_file(&mods.join("old.jar.disabled"))
                        .expect("disabled hash"),
                ),
                size: Some(b"disabled".len() as u64),
                primary: true,
            },
            Vec::new(),
            None,
        )
        .expect("valid managed entry"));

        assert!(!preserved_enabled_state(&root, &manifest, &id));
        fs::rename(mods.join("old.jar.disabled"), mods.join("old.jar"))
            .expect("enable existing mod");
        assert!(preserved_enabled_state(&root, &manifest, &id));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn uninstall_does_not_commit_when_the_managed_path_cannot_be_removed() {
        let root = std::env::temp_dir().join(format!(
            "axial-content-uninstall-failure-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock")
                .as_nanos()
        ));
        fs::create_dir_all(root.join("mods/managed.jar")).expect("managed path fixture");
        let id = CanonicalId::for_project(ProviderId::Modrinth, "project");
        let mut manifest = ContentManifest::default();
        insert(&mut manifest, ManifestEntry::managed(
            id.clone(),
            ProviderId::Modrinth,
            "project".to_string(),
            "version".to_string(),
            ContentKind::Mod,
            &FileRef {
                url: "https://example.invalid/managed.jar".to_string(),
                filename: "managed.jar".to_string(),
                sha1: None,
                sha512: Some("0".repeat(128)),
                size: Some(1),
                primary: true,
            },
            Vec::new(),
            None,
        )
        .expect("valid managed entry"));
        manifest.save(&root).expect("save manifest");

        assert!(uninstall(&root, &id).is_err());
        let persisted = ContentManifest::load(&root).expect("reload manifest");
        assert!(persisted.find(&id).is_some());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn uninstall_preserves_a_same_name_manual_replacement_and_provenance() {
        let root = test_root("uninstall-user-replacement");
        fs::create_dir_all(root.join("mods")).expect("mods");
        let path = root.join("mods/managed.jar");
        fs::write(&path, b"managed bytes").expect("managed file");
        let id = CanonicalId::for_project(ProviderId::Modrinth, "project");
        let mut entry = recorded("project", "managed.jar");
        entry.record_authenticated_file(
            b"managed bytes".len() as u64,
            crate::manifest::sha512_file(&path).expect("managed hash"),
        )
        .expect("record managed file");
        let mut manifest = ContentManifest::default();
        insert(&mut manifest, entry);
        manifest.save(&root).expect("save manifest");

        fs::write(&path, b"user replacement").expect("replace managed file");
        assert!(uninstall(&root, &id).is_err());

        assert_eq!(
            fs::read(&path).expect("preserved replacement"),
            b"user replacement"
        );
        assert!(
            ContentManifest::load(&root)
                .expect("reload manifest")
                .find(&id)
                .is_some()
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn removal_commit_preserves_a_destination_replaced_after_preflight() {
        let root = test_root("removal-replacement-after-preflight");
        fs::create_dir_all(root.join("mods")).expect("mods");
        let path = root.join("mods/managed.jar");
        fs::write(&path, b"managed bytes").expect("managed file");
        let mut entry = recorded("project", "managed.jar");
        entry.record_authenticated_file(
            b"managed bytes".len() as u64,
            crate::manifest::sha512_file(&path).expect("managed hash"),
        )
        .expect("record managed file");
        let mut manifest = ContentManifest::default();
        insert(&mut manifest, entry.clone());
        manifest.save(&root).expect("save manifest");
        let removals = verified_removable_variants(
            &root,
            &entry,
            &ProtectedManagedPaths::default(),
        )
        .expect("removal preflight");

        fs::write(&path, b"user replacement").expect("replace after preflight");
        let mut transaction = FileTransaction::empty(&root).expect("transaction");
        let error = stage_managed_removals(&mut transaction, &removals)
            .expect_err("claim-time ownership validation must reject replacement");

        assert!(error.to_string().contains("changed before removal commit"));
        assert_eq!(
            fs::read(&path).expect("preserved replacement"),
            b"user replacement"
        );
        assert_eq!(
            ContentManifest::load(&root).expect("reload manifest"),
            manifest
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn uninstall_accepts_either_exact_live_managed_variant() {
        for enabled in [true, false] {
            let root = test_root(if enabled {
                "uninstall-enabled-variant"
            } else {
                "uninstall-disabled-variant"
            });
            let manifest =
                save_managed_mod(&root, "project", "managed.jar", enabled, b"managed bytes");
            let id = manifest.entries()[0].canonical_id().clone();

            assert!(uninstall(&root, &id).expect("exact managed variant uninstalls"));
            assert!(!root.join("mods/managed.jar").exists());
            assert!(!root.join("mods/managed.jar.disabled").exists());
            assert!(ContentManifest::load(&root).expect("manifest").is_empty());

            let _ = fs::remove_dir_all(root);
        }
    }

    #[test]
    fn removal_commit_rejects_a_late_alias_for_an_absent_variant() {
        let root = test_root("removal-late-absent-alias");
        fs::create_dir_all(root.join("mods")).expect("mods");
        let enabled = root.join("mods/managed.jar");
        fs::write(&enabled, b"managed bytes").expect("managed file");
        let mut entry = recorded("project", "managed.jar");
        entry
            .record_authenticated_file(
                b"managed bytes".len() as u64,
                crate::manifest::sha512_file(&enabled).expect("managed hash"),
            )
            .expect("record managed file");
        let removals = verified_removable_variants(
            &root,
            &entry,
            &ProtectedManagedPaths::default(),
        )
        .expect("preflight");
        let late_alias = root.join("mods/MANAGED.JAR.DISABLED");
        fs::write(&late_alias, b"late user bytes").expect("late alias");

        let mut transaction = FileTransaction::empty(&root).expect("transaction");
        assert!(stage_managed_removals(&mut transaction, &removals).is_err());
        assert_eq!(fs::read(&enabled).expect("enabled retained"), b"managed bytes");
        assert_eq!(
            fs::read(&late_alias).expect("late alias retained"),
            b"late user bytes"
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn removal_commit_rejects_a_late_exact_managed_variant() {
        for late_name in ["managed.jar", "managed.jar.disabled"] {
            let root = test_root(if late_name.ends_with(".disabled") {
                "removal-late-disabled-variant"
            } else {
                "removal-late-enabled-variant"
            });
            fs::create_dir_all(root.join("mods")).expect("mods");
            let entry = recorded("project", "managed.jar");
            let removals = verified_removable_variants(
                &root,
                &entry,
                &ProtectedManagedPaths::default(),
            )
            .expect("absent preflight");
            let late_path = root.join("mods").join(late_name);
            fs::write(&late_path, b"late user bytes").expect("late exact variant");

            let mut transaction = FileTransaction::empty(&root).expect("transaction");
            let error = stage_managed_removals(&mut transaction, &removals)
                .expect_err("late exact variant must not be adopted");
            assert!(error.to_string().contains("unexpectedly became present"));
            assert_eq!(
                fs::read(&late_path).expect("late exact variant retained"),
                b"late user bytes"
            );

            let _ = fs::remove_dir_all(root);
        }
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

    #[test]
    fn uninstall_rejects_removing_a_live_required_dependency() {
        for version_only in [false, true] {
            let root = test_root(if version_only {
                "uninstall-required-version-only"
            } else {
                "uninstall-required-project"
            });
            fs::create_dir_all(root.join("mods")).expect("mods");
            fs::write(root.join("mods/dependency.jar"), b"dependency").expect("dependency file");
            fs::write(root.join("mods/dependent.jar"), b"dependent").expect("dependent file");

            let mut dependency = recorded("dependency", "dependency.jar");
            dependency
                .record_authenticated_file(
                    b"dependency".len() as u64,
                    crate::manifest::sha512_file(&root.join("mods/dependency.jar"))
                        .expect("dependency hash"),
                )
                .expect("record dependency file");
            let dependency_id = dependency.canonical_id().clone();
            let dependency_record = ContentDependency {
                project_id: (!version_only).then(|| dependency.project_id().to_string()),
                version_id: Some(dependency.version_id().to_string()),
                kind: crate::model::DependencyKind::Required,
            };
            let mut dependent = recorded_with_dependencies(
                "dependent",
                "dependent.jar",
                vec![dependency_record],
            );
            dependent
                .record_authenticated_file(
                    b"dependent".len() as u64,
                    crate::manifest::sha512_file(&root.join("mods/dependent.jar"))
                        .expect("dependent hash"),
                )
                .expect("record dependent file");
            let mut manifest = ContentManifest::default();
            insert(&mut manifest, dependency);
            insert(&mut manifest, dependent);
            manifest.save(&root).expect("save manifest");

            let error = uninstall(&root, &dependency_id)
                .expect_err("a live required dependency must not be removed");
            assert!(error.to_string().contains("required"));
            assert!(root.join("mods/dependency.jar").is_file());
            assert_eq!(
                ContentManifest::load(&root)
                    .expect("reload manifest")
                    .len(),
                2
            );
            let _ = fs::remove_dir_all(root);
        }
    }

    #[test]
    fn batch_uninstall_removes_dependents_and_dependencies_atomically() {
        let root = test_root("batch-uninstall-dependency-closure");
        fs::create_dir_all(root.join("mods")).expect("mods");
        fs::write(root.join("mods/dependency.jar"), b"dependency").expect("dependency file");
        fs::write(root.join("mods/dependent.jar"), b"dependent").expect("dependent file");

        let mut dependency = recorded("dependency", "dependency.jar");
        dependency
            .record_authenticated_file(
                b"dependency".len() as u64,
                crate::manifest::sha512_file(&root.join("mods/dependency.jar"))
                    .expect("dependency hash"),
            )
            .expect("record dependency file");
        let dependency_id = dependency.canonical_id().clone();
        let dependency_record = ContentDependency {
            project_id: Some(dependency.project_id().to_string()),
            version_id: Some(dependency.version_id().to_string()),
            kind: crate::model::DependencyKind::Required,
        };
        let mut dependent = recorded_with_dependencies(
            "dependent",
            "dependent.jar",
            vec![dependency_record],
        );
        dependent
            .record_authenticated_file(
                b"dependent".len() as u64,
                crate::manifest::sha512_file(&root.join("mods/dependent.jar"))
                    .expect("dependent hash"),
            )
            .expect("record dependent file");
        let dependent_id = dependent.canonical_id().clone();
        let mut manifest = ContentManifest::default();
        insert(&mut manifest, dependency);
        insert(&mut manifest, dependent);
        manifest.save(&root).expect("save manifest");

        let removed = uninstall_many(&root, &[dependency_id, dependent_id])
            .expect("the selected dependency closure should be removable in any input order");

        assert_eq!(removed, 2);
        assert!(!root.join("mods/dependency.jar").exists());
        assert!(!root.join("mods/dependent.jar").exists());
        assert!(
            ContentManifest::load(&root)
                .expect("reload manifest")
                .is_empty()
        );
        let _ = fs::remove_dir_all(root);
    }
}
