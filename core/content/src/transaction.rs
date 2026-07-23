use crate::error::{ContentError, ContentResult};
use crate::model::ContentKind;
use axial_minecraft::portable_path::{
    PortableFileName, PortablePathKey, PortableRelativePath, managed_content_name_is_reserved,
    managed_content_name_key,
};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static TRANSACTION_SEQUENCE: AtomicU64 = AtomicU64::new(1);
const MAX_PORTABLE_INVENTORY_ENTRIES: usize = 100_000;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ManagedContentInventory {
    parents: BTreeMap<Option<PortablePathKey>, ManagedContentParentInventory>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ManagedContentParentInventory {
    relative: Option<PortableRelativePath>,
    exists: bool,
    managed_names: bool,
    tracked_names: BTreeSet<PortablePathKey>,
    entries: BTreeMap<PortablePathKey, ManagedContentInventoryEntry>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ManagedContentInventoryEntry {
    name: String,
    file_type: ManagedContentFileType,
}

#[derive(Clone, Debug)]
struct PortableDirectoryEntry {
    name: PortableFileName,
    raw: String,
    path: PathBuf,
}

#[derive(Clone, Debug, Default)]
struct PortableDirectoryIndex {
    entries: Vec<PortableDirectoryEntry>,
    aliases: BTreeMap<PortablePathKey, Vec<usize>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ManagedContentFileType {
    File,
    Directory,
    Symlink,
    Other,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ManagedContentParent {
    Mods,
    ResourcePacks,
    ShaderPacks,
}

impl ManagedContentParent {
    pub(crate) fn kind(self) -> ContentKind {
        match self {
            Self::Mods => ContentKind::Mod,
            Self::ResourcePacks => ContentKind::ResourcePack,
            Self::ShaderPacks => ContentKind::ShaderPack,
        }
    }

    fn canonical(self) -> &'static str {
        match self {
            Self::Mods => "mods",
            Self::ResourcePacks => "resourcepacks",
            Self::ShaderPacks => "shaderpacks",
        }
    }
}

pub(crate) fn managed_content_parent(
    parent: Option<&PortableRelativePath>,
) -> ContentResult<Option<ManagedContentParent>> {
    let Some(parent) = parent.filter(|parent| !parent.as_str().contains('/')) else {
        return Ok(None);
    };
    for candidate in [
        ManagedContentParent::Mods,
        ManagedContentParent::ResourcePacks,
        ManagedContentParent::ShaderPacks,
    ] {
        let canonical = PortableRelativePath::new_exact(candidate.canonical())
            .expect("managed content parents are portable");
        if parent.key() == canonical.key() {
            if parent.as_str() != candidate.canonical() {
                return Err(ContentError::Invalid(
                    "managed content parent must use its exact canonical spelling".to_string(),
                ));
            }
            return Ok(Some(candidate));
        }
    }
    Ok(None)
}

impl ManagedContentInventory {
    pub(crate) fn capture(root: &Path, relative_paths: &[String]) -> ContentResult<Self> {
        let mut directory_cache = BTreeMap::new();
        let mut scan_budget = MAX_PORTABLE_INVENTORY_ENTRIES;
        let mut touched_parents = BTreeMap::<
            Option<PortablePathKey>,
            (Option<PortableRelativePath>, BTreeSet<PortablePathKey>),
        >::new();
        for relative in relative_paths {
            let (parent, name) = destination_parts(relative)?;
            let managed_names = managed_content_parent(parent.as_ref())?.is_some();
            let name_key = if managed_names {
                managed_content_name_key(&name)
            } else {
                name.key()
            };
            touched_parents
                .entry(parent.as_ref().map(PortableRelativePath::key))
                .or_insert_with(|| (parent, BTreeSet::new()))
                .1
                .insert(name_key);
        }
        let mut parents = BTreeMap::new();
        for (parent_key, (relative, touched_names)) in touched_parents {
            let managed_names = managed_content_parent(relative.as_ref())?.is_some();
            let Some(parent_path) = resolve_portable_parent(
                root,
                relative.as_ref(),
                &mut directory_cache,
                &mut scan_budget,
            )? else {
                parents.insert(
                    parent_key,
                    ManagedContentParentInventory {
                        relative,
                        exists: false,
                        managed_names,
                        tracked_names: touched_names,
                        entries: BTreeMap::new(),
                    },
                );
                continue;
            };
            let directory = directory_index(
                &parent_path,
                &mut directory_cache,
                &mut scan_budget,
            )?;
            let selected = if managed_names {
                directory.entries.iter().collect::<Vec<_>>()
            } else {
                touched_names
                    .iter()
                    .flat_map(|key| {
                        directory
                            .aliases
                            .get(key)
                            .into_iter()
                            .flatten()
                            .map(|index| &directory.entries[*index])
                    })
                    .collect::<Vec<_>>()
            };
            let mut entries = BTreeMap::new();
            for entry in selected {
                let key = if managed_names {
                    managed_content_name_key(&entry.name)
                } else {
                    entry.name.key()
                };
                let metadata = fs::symlink_metadata(&entry.path)?;
                let file_type = if metadata.file_type().is_symlink() {
                    ManagedContentFileType::Symlink
                } else if metadata.is_file() {
                    ManagedContentFileType::File
                } else if metadata.is_dir() {
                    ManagedContentFileType::Directory
                } else {
                    ManagedContentFileType::Other
                };
                if entries
                    .insert(
                        key,
                        ManagedContentInventoryEntry {
                            name: entry.raw.clone(),
                            file_type,
                        },
                    )
                    .is_some()
                {
                    return Err(ContentError::Invalid(
                        "a touched content directory contains portable path aliases".to_string(),
                    ));
                }
            }
            parents.insert(
                parent_key,
                ManagedContentParentInventory {
                    relative,
                    exists: true,
                    managed_names,
                    tracked_names: touched_names,
                    entries,
                },
            );
        }
        Ok(Self { parents })
    }

    pub(crate) fn require_exact_or_absent(&self, relative: &str) -> ContentResult<bool> {
        let (parent, name) = destination_parts(relative)?;
        let parent_key = parent.as_ref().map(PortableRelativePath::key);
        let Some(parent_inventory) = self.parents.get(&parent_key) else {
            return Ok(false);
        };
        let name_key = if parent_inventory.managed_names {
            managed_content_name_key(&name)
        } else {
            name.key()
        };
        let Some(existing) = parent_inventory.entries.get(&name_key) else {
            return Ok(false);
        };
        if existing.name != name.as_str() {
            return Err(ContentError::Invalid(
                "a content destination has a portable path alias".to_string(),
            ));
        }
        Ok(true)
    }

    pub(crate) fn require_exact_managed_file_variant_or_absent(
        &self,
        enabled_relative: &str,
        disabled_relative: &str,
    ) -> ContentResult<bool> {
        let (enabled_parent, enabled_name) = destination_parts(enabled_relative)?;
        let (disabled_parent, disabled_name) = destination_parts(disabled_relative)?;
        let parent_key = enabled_parent.as_ref().map(PortableRelativePath::key);
        let expected_disabled = enabled_name.with_suffix(".disabled").map_err(|_| {
            ContentError::Invalid("managed content variants have an invalid spelling".to_string())
        })?;
        if parent_key != disabled_parent.as_ref().map(PortableRelativePath::key)
            || expected_disabled != disabled_name
            || managed_content_name_key(&enabled_name) != enabled_name.key()
            || managed_content_name_is_reserved(&enabled_name)
        {
            return Err(ContentError::Invalid(
                "managed content variants do not describe one destination".to_string(),
            ));
        }
        let Some(parent_inventory) = self.parents.get(&parent_key) else {
            return Ok(false);
        };
        if !parent_inventory.managed_names {
            return Err(ContentError::Invalid(
                "managed content variants are outside a managed content directory".to_string(),
            ));
        }
        let name_key = managed_content_name_key(&enabled_name);
        let Some(existing) = parent_inventory.entries.get(&name_key) else {
            return Ok(false);
        };
        if existing.name != enabled_name.as_str() && existing.name != disabled_name.as_str() {
            return Err(ContentError::Invalid(
                "a content destination has a portable path alias".to_string(),
            ));
        }
        if existing.file_type != ManagedContentFileType::File {
            return Err(ContentError::Invalid(
                "a managed content destination is not a regular file".to_string(),
            ));
        }
        Ok(true)
    }

    fn record_file(&mut self, relative: &str) -> ContentResult<()> {
        self.record_parent_directories(relative)?;
        let (parent, name) = destination_parts(relative)?;
        let parent_key = parent.as_ref().map(PortableRelativePath::key);
        let inventory = self.parents.get_mut(&parent_key).ok_or_else(|| {
            ContentError::Invalid("content transaction path was not inventoried".to_string())
        })?;
        inventory.exists = true;
        let name_key = if inventory.managed_names {
            managed_content_name_key(&name)
        } else {
            name.key()
        };
        inventory.entries.insert(
            name_key,
            ManagedContentInventoryEntry {
                name: name.to_string(),
                file_type: ManagedContentFileType::File,
            },
        );
        Ok(())
    }

    fn record_parent_directories(&mut self, relative: &str) -> ContentResult<()> {
        let relative = PortableRelativePath::new_exact(relative)
            .map_err(|_| ContentError::Invalid("content file path is invalid".to_string()))?;
        let components = relative.as_str().split('/').collect::<Vec<_>>();
        for index in 0..components.len().saturating_sub(1) {
            let parent = if index == 0 {
                None
            } else {
                Some(
                    PortableRelativePath::new_exact(&components[..index].join("/"))
                        .expect("portable path prefixes remain portable"),
                )
            };
            let parent_key = parent.as_ref().map(PortableRelativePath::key);
            let Some(inventory) = self.parents.get_mut(&parent_key) else {
                continue;
            };
            inventory.exists = true;
            let name = PortableFileName::new_exact(components[index])
                .expect("portable path components remain portable");
            let name_key = if inventory.managed_names {
                managed_content_name_key(&name)
            } else {
                name.key()
            };
            if !inventory.managed_names && !inventory.tracked_names.contains(&name_key) {
                continue;
            }
            inventory.entries.insert(
                name_key,
                ManagedContentInventoryEntry {
                    name: name.to_string(),
                    file_type: ManagedContentFileType::Directory,
                },
            );
        }
        Ok(())
    }

    fn record_absent(&mut self, relative: &str) -> ContentResult<()> {
        let (parent, name) = destination_parts(relative)?;
        let parent_key = parent.as_ref().map(PortableRelativePath::key);
        let inventory = self.parents.get_mut(&parent_key).ok_or_else(|| {
            ContentError::Invalid("content transaction path was not inventoried".to_string())
        })?;
        let name_key = if inventory.managed_names {
            managed_content_name_key(&name)
        } else {
            name.key()
        };
        inventory.entries.remove(&name_key);
        Ok(())
    }

    fn verify(&self, root: &Path, relative_paths: &[String]) -> ContentResult<()> {
        let current = Self::capture(root, relative_paths)?;
        if current == *self {
            Ok(())
        } else {
            Err(ContentError::Invalid(
                "a touched content directory changed before commit".to_string(),
            ))
        }
    }

    pub(crate) fn expand(
        &self,
        root: &Path,
        relative_paths: &[String],
    ) -> ContentResult<Self> {
        let expanded = Self::capture(root, relative_paths)?;
        for (key, expected) in &self.parents {
            let Some(actual) = expanded.parents.get(key) else {
                return Err(ContentError::Invalid(
                    "a touched content directory changed before commit".to_string(),
                ));
            };
            let identity_matches = actual.relative == expected.relative
                && actual.exists == expected.exists
                && actual.managed_names == expected.managed_names;
            let observations_match = if expected.managed_names {
                actual.entries == expected.entries
            } else {
                expected.tracked_names.iter().all(|name| {
                    actual.tracked_names.contains(name)
                        && actual.entries.get(name) == expected.entries.get(name)
                })
            };
            if !identity_matches || !observations_match {
                return Err(ContentError::Invalid(
                    "a touched content directory changed before commit".to_string(),
                ));
            }
        }
        Ok(expanded)
    }
}

fn destination_parts(
    relative: &str,
) -> ContentResult<(Option<PortableRelativePath>, PortableFileName)> {
    let relative = PortableRelativePath::new_exact(relative)
        .map_err(|_| ContentError::Invalid("content file path is invalid".to_string()))?;
    let (parent, name) = match relative.as_str().rsplit_once('/') {
        Some((parent, name)) => (
            Some(PortableRelativePath::new_exact(parent).map_err(|_| {
                ContentError::Invalid("content parent path is invalid".to_string())
            })?),
            name,
        ),
        None => (None, relative.as_str()),
    };
    let name = PortableFileName::new_exact(name)
        .map_err(|_| ContentError::Invalid("content filename is invalid".to_string()))?;
    Ok((parent, name))
}

fn resolve_portable_parent(
    root: &Path,
    relative: Option<&PortableRelativePath>,
    directory_cache: &mut BTreeMap<PathBuf, PortableDirectoryIndex>,
    scan_budget: &mut usize,
) -> ContentResult<Option<PathBuf>> {
    let mut current = root.to_path_buf();
    let Some(relative) = relative else {
        return Ok(Some(current));
    };
    for component in relative.as_str().split('/') {
        let expected = PortableFileName::new_exact(component)
            .expect("portable path components are portable filenames");
        let directory = directory_index(&current, directory_cache, scan_budget)?;
        let aliases = directory
            .aliases
            .get(&expected.key())
            .map(Vec::as_slice)
            .unwrap_or_default();
        if aliases.len() > 1
            || aliases
                .first()
                .is_some_and(|index| directory.entries[*index].raw != expected.as_str())
        {
            return Err(ContentError::Invalid(
                "a touched content parent has a portable path alias".to_string(),
            ));
        }
        let Some(index) = aliases.first() else {
            return Ok(None);
        };
        let entry = &directory.entries[*index];
        let metadata = fs::symlink_metadata(&entry.path)?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err(ContentError::Invalid(
                "a touched content parent is not a regular directory".to_string(),
            ));
        }
        current = entry.path.clone();
    }
    Ok(Some(current))
}

fn directory_index<'a>(
    path: &Path,
    cache: &'a mut BTreeMap<PathBuf, PortableDirectoryIndex>,
    scan_budget: &mut usize,
) -> ContentResult<&'a PortableDirectoryIndex> {
    if !cache.contains_key(path) {
        let index = read_directory_bounded(path, scan_budget)?;
        cache.insert(path.to_path_buf(), index);
    }
    Ok(cache
        .get(path)
        .expect("bounded directory inventory was cached"))
}

fn read_directory_bounded(
    path: &Path,
    scan_budget: &mut usize,
) -> ContentResult<PortableDirectoryIndex> {
    let entries = match fs::read_dir(path) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(PortableDirectoryIndex::default());
        }
        Err(error) => return Err(ContentError::Io(error)),
    };
    let mut result = PortableDirectoryIndex::default();
    for entry in entries {
        let Some(remaining) = scan_budget.checked_sub(1) else {
            return Err(ContentError::Invalid(
                "content inventory exceeds its aggregate entry bound".to_string(),
            ));
        };
        *scan_budget = remaining;
        let entry = entry?;
        let Ok(raw) = entry.file_name().into_string() else {
            continue;
        };
        let Some(name) = portable_alias_name(&raw) else {
            continue;
        };
        let key = name.key();
        let index = result.entries.len();
        result.entries.push(PortableDirectoryEntry {
            name,
            raw,
            path: entry.path(),
        });
        result.aliases.entry(key).or_default().push(index);
    }
    Ok(result)
}

fn portable_alias_name(raw: &str) -> Option<PortableFileName> {
    PortableFileName::new(raw).ok().or_else(|| {
        let trimmed = raw.trim_end_matches(['.', ' ']);
        (trimmed != raw)
            .then(|| PortableFileName::new(trimmed).ok())
            .flatten()
    })
}

pub(crate) fn staging_dir(root: &Path, prefix: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let sequence = TRANSACTION_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    root.join(format!(".{prefix}-{nanos:x}-{sequence:x}"))
}

pub(crate) struct StagingGuard {
    path: PathBuf,
    transferred: bool,
}

impl StagingGuard {
    pub(crate) fn create(root: &Path, prefix: &str) -> ContentResult<Self> {
        let path = staging_dir(root, prefix);
        fs::create_dir_all(&path)?;
        Ok(Self {
            path,
            transferred: false,
        })
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn transfer(mut self) -> PathBuf {
        self.transferred = true;
        self.path.clone()
    }
}

impl Drop for StagingGuard {
    fn drop(&mut self) {
        if !self.transferred {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

pub(crate) fn contained_path(root: &Path, relative: &str) -> ContentResult<PathBuf> {
    let candidate = PortableRelativePath::new_exact(relative)
        .map_err(|_| ContentError::Invalid("content file path is invalid".to_string()))?;
    reject_symlink(root)?;
    let mut resolved = root.to_path_buf();
    for component in candidate.as_str().split('/') {
        resolved.push(component);
        reject_symlink(&resolved)?;
    }
    Ok(resolved)
}

fn reject_symlink(path: &Path) -> ContentResult<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(ContentError::Invalid(
            "content path contains a symbolic link".to_string(),
        )),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(ContentError::Io(error)),
    }
}

/// Promote a temporary file over an existing destination on every supported
/// platform. Windows rename does not replace files, so the old destination is
/// first moved aside and restored if promotion fails.
pub(crate) fn promote_replacement(source: &Path, destination: &Path) -> ContentResult<()> {
    let first_error = match fs::rename(source, destination) {
        Ok(()) => return Ok(()),
        Err(error) => error,
    };
    promote_replacement_after_rename_failure(source, destination, first_error)
}

fn promote_replacement_after_rename_failure(
    source: &Path,
    destination: &Path,
    first_error: std::io::Error,
) -> ContentResult<()> {
    match fs::symlink_metadata(source) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(ContentError::Io(first_error));
        }
        Err(error) => return Err(ContentError::Io(error)),
    }
    match fs::symlink_metadata(destination) {
        Ok(metadata) if metadata.is_file() || metadata.file_type().is_symlink() => {}
        Ok(_) => return Err(ContentError::Io(first_error)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(ContentError::Io(first_error));
        }
        Err(error) => return Err(ContentError::Io(error)),
    }

    let parent = destination.parent().unwrap_or_else(|| Path::new("."));
    let backup = staging_dir(parent, "axial-replacement-backup");
    fs::rename(destination, &backup)?;
    match fs::rename(source, destination) {
        Ok(()) => {
            let _ = fs::remove_file(backup);
            Ok(())
        }
        Err(error) => {
            let restore = fs::rename(&backup, destination);
            match restore {
                Ok(()) => Err(ContentError::Io(error)),
                Err(restore_error) => Err(ContentError::Io(std::io::Error::other(format!(
                    "failed to promote replacement: {error}; failed to restore destination: {restore_error}"
                )))),
            }
        }
    }
}

pub(crate) struct FileTransaction {
    root: PathBuf,
    staging: PathBuf,
    backup: PathBuf,
    applied: Vec<AppliedFile>,
    removed: Vec<String>,
    guarded_paths: Vec<String>,
    managed_inventory: ManagedContentInventory,
    preserve_staging: bool,
    finished: bool,
}

#[derive(Debug, Clone)]
struct AppliedFile {
    relative: String,
    expected: PathBuf,
}

impl FileTransaction {
    pub(crate) fn apply_new_with_inventory(
        root: &Path,
        staging: PathBuf,
        relative_paths: &[String],
        guarded_paths: &[String],
        inventory: ManagedContentInventory,
    ) -> ContentResult<Self> {
        let backup = staging.join(".backup");
        inventory.verify(root, guarded_paths)?;
        for relative in relative_paths {
            inventory.require_exact_or_absent(relative)?;
        }
        let mut transaction = Self {
            root: root.to_path_buf(),
            staging,
            backup,
            applied: Vec::new(),
            removed: Vec::new(),
            guarded_paths: guarded_paths.to_vec(),
            managed_inventory: inventory,
            preserve_staging: false,
            finished: false,
        };
        for relative in relative_paths {
            if let Err(error) = transaction.apply_one(relative) {
                if let Err(rollback_error) = transaction.rollback_inner() {
                    transaction.finished = true;
                    return Err(rollback_error);
                }
                transaction.finished = true;
                return Err(error);
            }
        }
        Ok(transaction)
    }

    pub(crate) fn empty(root: &Path) -> ContentResult<Self> {
        let staging = StagingGuard::create(root, "axial-content-transaction")?;
        let staging = staging.transfer();
        Ok(Self {
            root: root.to_path_buf(),
            backup: staging.join(".backup"),
            staging,
            applied: Vec::new(),
            removed: Vec::new(),
            guarded_paths: Vec::new(),
            managed_inventory: ManagedContentInventory::capture(root, &[])?,
            preserve_staging: false,
            finished: false,
        })
    }

    /// Claim existing destinations into the transaction backup and validate the
    /// claimed bytes before removal can become part of the transaction. If a
    /// later claim fails, earlier removals remain staged and are restored by an
    /// explicit rollback or by dropping the transaction.
    pub(crate) fn stage_removals_with_revalidation<F>(
        &mut self,
        relative_paths: &[String],
        mut validate_claimed: F,
    ) -> ContentResult<()>
    where
        F: FnMut(&str, &Path) -> ContentResult<()>,
    {
        self.guard_additional_paths(relative_paths)?;
        for relative in relative_paths {
            self.stage_removal(relative, &mut validate_claimed)?;
        }
        Ok(())
    }

    /// Atomically claim `source`, classify the claimed bytes, and publish an
    /// identical file at an absent `target`. Rollback compares the published
    /// bytes with the retained claim before removing them and restores the
    /// source without replacing a path that appeared in the meantime.
    pub(crate) fn move_new_with_revalidation<T, F, P>(
        &mut self,
        source: &str,
        target: &str,
        validate_claimed: F,
        before_publish: P,
    ) -> ContentResult<T>
    where
        F: FnOnce(&Path) -> ContentResult<T>,
        P: FnOnce(),
    {
        self.guard_additional_paths(&[source.to_string(), target.to_string()])?;
        if source == target
            || self
                .applied
                .iter()
                .any(|applied| applied.relative == source || applied.relative == target)
            || self
                .removed
                .iter()
                .any(|removed| removed == source || removed == target)
        {
            return Err(ContentError::Invalid(
                "content move overlaps another transaction path".to_string(),
            ));
        }

        let source_path = contained_path(&self.root, source)?;
        let target_path = contained_path(&self.root, target)?;
        match fs::symlink_metadata(&source_path) {
            Ok(metadata) if metadata.is_file() => {}
            Ok(_) => {
                return Err(ContentError::Invalid(
                    "content move source is not a regular file".to_string(),
                ));
            }
            Err(error) => return Err(ContentError::Io(error)),
        }
        match fs::symlink_metadata(&target_path) {
            Ok(_) => {
                return Err(ContentError::Invalid(
                    "content destination became occupied before commit".to_string(),
                ));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(ContentError::Io(error)),
        }

        let claimed = contained_path(&self.backup, source)?;
        if let Some(parent) = claimed.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::rename(&source_path, &claimed)?;
        let validation = match validate_claimed(&claimed) {
            Ok(validation) => validation,
            Err(error) => {
                return Err(self.restore_claimed_or_retain(&claimed, &source_path, error));
            }
        };

        before_publish();
        let publish_result = promote_new_file_retaining_source(&claimed, &target_path);
        if let Err(error) = publish_result {
            return Err(self.restore_claimed_or_retain(&claimed, &source_path, error));
        }
        self.removed.push(source.to_string());
        self.applied.push(AppliedFile {
            relative: target.to_string(),
            expected: claimed,
        });
        self.managed_inventory.record_absent(source)?;
        self.managed_inventory.record_file(target)?;
        Ok(validation)
    }

    fn stage_removal<F>(&mut self, relative: &str, validate_claimed: &mut F) -> ContentResult<()>
    where
        F: FnMut(&str, &Path) -> ContentResult<()>,
    {
        if self
            .applied
            .iter()
            .any(|applied| applied.relative == relative)
            || self.removed.iter().any(|removed| removed == relative)
        {
            return Ok(());
        }
        let destination = contained_path(&self.root, relative)?;
        match fs::symlink_metadata(&destination) {
            Ok(metadata) if metadata.is_dir() => {
                return Err(ContentError::Invalid(format!(
                    "content destination is a directory: {relative}"
                )));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(ContentError::Io(error)),
        }
        let backup = contained_path(&self.backup, relative)?;
        if let Some(parent) = backup.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::rename(&destination, &backup)?;
        if let Err(error) = validate_claimed(relative, &backup) {
            return Err(self.restore_claimed_or_retain(&backup, &destination, error));
        }
        self.removed.push(relative.to_string());
        self.managed_inventory.record_absent(relative)?;
        Ok(())
    }

    fn apply_one(&mut self, relative: &str) -> ContentResult<()> {
        self.managed_inventory.require_exact_or_absent(relative)?;
        let staged = contained_path(&self.staging, relative)?;
        let destination = contained_path(&self.root, relative)?;
        if destination.is_dir() {
            return Err(ContentError::Invalid(format!(
                "content destination is a directory: {relative}"
            )));
        }
        let existed = match fs::symlink_metadata(&destination) {
            Ok(_) => true,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(error) => return Err(ContentError::Io(error)),
        };
        if existed {
            return Err(ContentError::Invalid(
                "content destination became occupied before commit".to_string(),
            ));
        }
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)?;
        }
        let promote_result = promote_new_file_retaining_source(&staged, &destination);
        if let Err(error) = promote_result {
            return Err(error);
        }
        self.applied.push(AppliedFile {
            relative: relative.to_string(),
            expected: staged,
        });
        self.managed_inventory.record_file(relative)?;
        Ok(())
    }

    pub(crate) fn verify_managed_inventory(&self) -> ContentResult<()> {
        self.managed_inventory
            .verify(&self.root, &self.guarded_paths)
    }

    pub(crate) fn guard_additional_paths(
        &mut self,
        relative_paths: &[String],
    ) -> ContentResult<()> {
        let mut known_paths = self.guarded_paths.iter().cloned().collect::<HashSet<_>>();
        if relative_paths
            .iter()
            .all(|relative| known_paths.contains(relative))
        {
            return Ok(());
        }
        let mut expanded_paths = self.guarded_paths.clone();
        for relative in relative_paths {
            if known_paths.insert(relative.clone()) {
                expanded_paths.push(relative.clone());
            }
        }
        let expanded = self
            .managed_inventory
            .expand(&self.root, &expanded_paths)?;
        for relative in relative_paths {
            expanded.require_exact_or_absent(relative)?;
        }
        self.guarded_paths = expanded_paths;
        self.managed_inventory = expanded;
        Ok(())
    }

    pub(crate) fn guard_managed_file_variants(
        &mut self,
        variants: &[(String, String)],
    ) -> ContentResult<()> {
        let mut expanded_paths = self.guarded_paths.clone();
        let mut known_paths = expanded_paths.iter().cloned().collect::<HashSet<_>>();
        for (enabled, disabled) in variants {
            for relative in [enabled, disabled] {
                if known_paths.insert(relative.clone()) {
                    expanded_paths.push(relative.clone());
                }
            }
        }
        let expanded = if expanded_paths == self.guarded_paths {
            self.managed_inventory.clone()
        } else {
            self.managed_inventory.expand(&self.root, &expanded_paths)?
        };
        for (enabled, disabled) in variants {
            expanded.require_exact_managed_file_variant_or_absent(enabled, disabled)?;
        }
        self.guarded_paths = expanded_paths;
        self.managed_inventory = expanded;
        Ok(())
    }

    fn restore_claimed_or_retain(
        &mut self,
        backup: &Path,
        destination: &Path,
        original_error: ContentError,
    ) -> ContentError {
        match promote_new_file(backup, destination) {
            Ok(()) => original_error,
            Err(_) => {
                self.preserve_staging = true;
                ContentError::Invalid(
                    "content changed before commit and recovery bytes were retained because the destination became occupied"
                        .to_string(),
                )
            }
        }
    }

    pub(crate) fn commit(mut self) -> ContentResult<()> {
        self.verify_managed_inventory()?;
        self.finish_commit();
        Ok(())
    }

    pub(crate) fn commit_after_verified_publication(mut self) {
        self.finish_commit();
    }

    fn finish_commit(&mut self) {
        self.finished = true;
        if !self.preserve_staging {
            let _ = fs::remove_dir_all(&self.staging);
        }
    }

    pub(crate) fn rollback(mut self) -> ContentResult<()> {
        let result = self.rollback_inner();
        self.finished = true;
        result
    }

    fn rollback_inner(&mut self) -> ContentResult<()> {
        let mut failed = false;
        let applied = self.applied.clone();
        for applied in applied.iter().rev() {
            if self.rollback_applied(applied).is_err() {
                self.preserve_staging = true;
                failed = true;
            }
        }
        let removed = self.removed.clone();
        for relative in removed.iter().rev() {
            if let (Ok(destination), Ok(backup)) = (
                contained_path(&self.root, relative),
                contained_path(&self.backup, relative),
            ) && restore_without_clobber(&backup, &destination).is_err()
            {
                self.preserve_staging = true;
                failed = true;
            }
        }
        if !self.preserve_staging {
            let _ = fs::remove_dir_all(&self.staging);
        }
        if failed {
            Err(ContentError::Invalid(
                "content rollback could not restore every path without replacing newer filesystem changes; recovery bytes were retained"
                    .to_string(),
            ))
        } else {
            Ok(())
        }
    }

    fn rollback_applied(&mut self, applied: &AppliedFile) -> ContentResult<()> {
        let destination = contained_path(&self.root, &applied.relative)?;
        let rollback_claim =
            contained_path(&self.staging.join(".rollback-current"), &applied.relative)?;
        let current_exists = match fs::symlink_metadata(&destination) {
            Ok(_) => true,
            Err(error) if error.kind() == io::ErrorKind::NotFound => false,
            Err(error) => return Err(ContentError::Io(error)),
        };

        if !current_exists {
            return Ok(());
        }
        if let Some(parent) = rollback_claim.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::rename(&destination, &rollback_claim)?;
        if !regular_files_match(&rollback_claim, &applied.expected)? {
            self.preserve_staging = true;
            restore_without_clobber(&rollback_claim, &destination)?;
            return Err(ContentError::Invalid(
                "an applied content destination changed before rollback".to_string(),
            ));
        }
        fs::remove_file(&rollback_claim)?;
        Ok(())
    }
}

fn restore_without_clobber(claimed: &Path, destination: &Path) -> ContentResult<()> {
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
    }
    promote_new_file(claimed, destination)
}

fn regular_files_match(left: &Path, right: &Path) -> ContentResult<bool> {
    let left_metadata = fs::symlink_metadata(left)?;
    let right_metadata = fs::symlink_metadata(right)?;
    if !left_metadata.is_file()
        || !right_metadata.is_file()
        || left_metadata.len() != right_metadata.len()
    {
        return Ok(false);
    }
    let mut left = fs::File::open(left)?;
    let mut right = fs::File::open(right)?;
    let mut left_buffer = [0_u8; 64 * 1024];
    let mut right_buffer = [0_u8; 64 * 1024];
    loop {
        let left_read = left.read(&mut left_buffer)?;
        let right_read = right.read(&mut right_buffer)?;
        if left_read != right_read || left_buffer[..left_read] != right_buffer[..right_read] {
            return Ok(false);
        }
        if left_read == 0 {
            return Ok(true);
        }
    }
}

/// Promote a staged regular file without ever replacing an occupied path. A
/// hard link provides an atomic same-volume fast path. The fallback first
/// copies into a unique private directory beside the destination, then
/// publishes the completed copy atomically without replacing an occupied path.
fn promote_new_file(staged: &Path, destination: &Path) -> ContentResult<()> {
    promote_new_file_with_source_policy(staged, destination, true)
}

fn promote_new_file_retaining_source(staged: &Path, destination: &Path) -> ContentResult<()> {
    promote_new_file_with_source_policy(staged, destination, false)
}

fn promote_new_file_with_source_policy(
    staged: &Path,
    destination: &Path,
    remove_source: bool,
) -> ContentResult<()> {
    promote_new_file_with_copy(staged, destination, remove_source, |source, destination| {
        fs::copy(source, destination)
    })
}

fn promote_new_file_with_copy<F>(
    staged: &Path,
    destination: &Path,
    remove_source: bool,
    copy_file: F,
) -> ContentResult<()>
where
    F: FnOnce(&Path, &Path) -> io::Result<u64>,
{
    if remove_source {
        match fs::hard_link(staged, destination) {
            Ok(()) => {
                let _ = fs::remove_file(staged);
                return Ok(());
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                return Err(ContentError::Invalid(
                    "content destination became occupied before commit".to_string(),
                ));
            }
            Err(_) => {}
        }
    }

    let parent = destination.parent().unwrap_or_else(|| Path::new("."));
    let copy_root = staging_dir(parent, "axial-content-promotion");
    fs::create_dir(&copy_root)?;
    let private_copy = copy_root.join("payload");
    if let Err(error) = copy_file(staged, &private_copy) {
        let _ = fs::remove_dir_all(&copy_root);
        return Err(ContentError::Io(error));
    }
    let publish_result = publish_private_copy(&private_copy, destination);
    let _ = fs::remove_dir_all(&copy_root);
    publish_result?;
    if remove_source {
        let _ = fs::remove_file(staged);
    }
    Ok(())
}

fn publish_private_copy(private_copy: &Path, destination: &Path) -> ContentResult<()> {
    match fs::hard_link(private_copy, destination) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Err(ContentError::Invalid(
            "content destination became occupied before commit".to_string(),
        )),
        Err(error) => Err(ContentError::Io(error)),
    }
}

impl Drop for FileTransaction {
    fn drop(&mut self) {
        if !self.finished {
            let _ = self.rollback_inner();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "axial-content-transaction-{name}-{}",
            TRANSACTION_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("create fixture root");
        root
    }

    #[cfg(unix)]
    #[test]
    fn managed_inventory_rejects_portable_content_aliases() {
        for (fixture, alias) in [
            ("case-file", "EXAMPLE.JAR"),
            ("nfc-file", "e\u{301}.jar"),
            ("full-fold-file", "Straße.jar"),
            ("disabled", "example.jar.disabled"),
            ("repeated-disabled", "example.jar.disabled.disabled"),
        ] {
            let root = root(fixture);
            fs::create_dir(root.join("mods")).expect("mods");
            fs::write(root.join("mods").join(alias), b"alias").expect("alias file");
            let requested = match fixture {
                "nfc-file" => "mods/é.jar",
                "full-fold-file" => "mods/strasse.jar",
                _ => "mods/example.jar",
            };

            let inventory = ManagedContentInventory::capture(&root, &[requested.to_string()])
                .expect("capture aliased inventory");
            assert!(
                inventory.require_exact_or_absent(requested).is_err(),
                "accepted portable content alias {alias:?}"
            );
            let _ = fs::remove_dir_all(root);
        }
    }

    #[cfg(unix)]
    #[test]
    fn managed_inventory_rejects_parent_aliases_but_ignores_unrelated_root_names() {
        let aliased = root("parent-alias");
        fs::create_dir(aliased.join("mods.")).expect("aliased mods parent");
        assert!(
            ManagedContentInventory::capture(
                &aliased,
                &["mods/example.jar".to_string()]
            )
            .is_err()
        );
        let _ = fs::remove_dir_all(aliased);

        let unrelated = root("unrelated-invalid-name");
        fs::create_dir(unrelated.join("mods")).expect("mods");
        fs::write(unrelated.join("bad:name"), b"unrelated").expect("unrelated entry");
        ManagedContentInventory::capture(
            &unrelated,
            &["mods/example.jar".to_string()],
        )
        .expect("unrelated unportable root entry is outside the touched key");
        let _ = fs::remove_dir_all(unrelated);
    }

    #[test]
    fn nonmanaged_inventory_freezes_only_touched_keys() {
        let root = root("targeted-override-inventory");
        fs::create_dir(root.join("config")).expect("config");
        fs::write(root.join("config/selected.txt"), b"selected").expect("selected");
        let paths = vec!["config/selected.txt".to_string()];
        let inventory = ManagedContentInventory::capture(&root, &paths).expect("inventory");

        fs::write(root.join("config/unrelated.txt"), b"unrelated").expect("unrelated");
        inventory
            .verify(&root, &paths)
            .expect("unrelated override entry is outside the frozen key");
        fs::write(root.join("config/selected.txt"), b"changed metadata shape")
            .expect("change selected entry");
        inventory
            .verify(&root, &paths)
            .expect("regular-file contents are owned by authenticated effect validation");
        fs::remove_file(root.join("config/selected.txt")).expect("remove selected");
        assert!(inventory.verify(&root, &paths).is_err());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn nonmanaged_inventory_can_expand_with_an_unchanged_sibling_key() {
        let root = root("expand-targeted-override-inventory");
        fs::create_dir(root.join("config")).expect("config");
        fs::write(root.join("config/first.txt"), b"first").expect("first");
        fs::write(root.join("config/second.txt"), b"second").expect("second");
        let initial_paths = vec!["config/first.txt".to_string()];
        let expanded_paths = vec![
            "config/first.txt".to_string(),
            "config/second.txt".to_string(),
        ];
        let initial = ManagedContentInventory::capture(&root, &initial_paths).expect("initial");

        let expanded = initial
            .expand(&root, &expanded_paths)
            .expect("expand unchanged parent with a sibling key");

        assert!(expanded.require_exact_or_absent("config/first.txt").unwrap());
        assert!(expanded.require_exact_or_absent("config/second.txt").unwrap());
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn final_inventory_check_rejects_a_post_effect_managed_alias() {
        let root = root("post-effect-alias");
        fs::create_dir(root.join("mods")).expect("mods");
        let staging = StagingGuard::create(&root, "stage").expect("stage");
        fs::create_dir_all(staging.path().join("mods")).expect("staged mods");
        fs::write(staging.path().join("mods/example.jar"), b"managed").expect("staged file");
        let paths = vec!["mods/example.jar".to_string()];
        let inventory = ManagedContentInventory::capture(&root, &paths).expect("inventory");
        let transaction = FileTransaction::apply_new_with_inventory(
            &root,
            staging.transfer(),
            &paths,
            &paths,
            inventory,
        )
        .expect("apply");

        fs::write(root.join("mods/example.jar.disabled.disabled"), b"alias")
            .expect("late alias");
        assert!(transaction.verify_managed_inventory().is_err());
        drop(transaction);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn rollback_restores_staged_removals() {
        let root = root("remove-rollback");
        fs::create_dir_all(root.join("mods")).expect("mods");
        fs::write(root.join("mods/example.jar"), b"content").expect("content file");
        let mut transaction = FileTransaction::empty(&root).expect("transaction");

        transaction
            .stage_removals_with_revalidation(&["mods/example.jar".to_string()], |_, _| Ok(()))
            .expect("stage removal");
        assert!(!root.join("mods/example.jar").exists());
        transaction.rollback().expect("rollback");

        assert_eq!(
            fs::read(root.join("mods/example.jar")).expect("restored"),
            b"content"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn partial_removal_staging_is_restored_by_caller_rollback() {
        let root = root("partial-removal-staging");
        fs::create_dir_all(root.join("mods")).expect("mods");
        let first = root.join("mods/first.jar");
        let second = root.join("mods/second.jar");
        fs::write(&first, b"first").expect("first");
        fs::write(&second, b"second").expect("second");
        let mut transaction = FileTransaction::empty(&root).expect("transaction");

        let result = transaction.stage_removals_with_revalidation(
            &["mods/first.jar".to_string(), "mods/second.jar".to_string()],
            |relative, _| {
                if relative == "mods/second.jar" {
                    Err(ContentError::Invalid("reject second removal".to_string()))
                } else {
                    Ok(())
                }
            },
        );

        assert!(result.is_err());
        assert!(!first.exists());
        assert_eq!(fs::read(&second).expect("restored second"), b"second");
        transaction.rollback().expect("caller rollback");
        assert_eq!(fs::read(&first).expect("restored first"), b"first");
        assert_eq!(fs::read(&second).expect("preserved second"), b"second");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn removal_rollback_preserves_a_new_destination_and_retains_the_backup() {
        let root = root("remove-rollback-conflict");
        fs::create_dir_all(root.join("mods")).expect("mods");
        let destination = root.join("mods/example.jar");
        fs::write(&destination, b"removed bytes").expect("removal source");
        let mut transaction = FileTransaction::empty(&root).expect("transaction");
        let staging_root = transaction.staging.clone();
        transaction
            .stage_removals_with_revalidation(&["mods/example.jar".to_string()], |_, _| Ok(()))
            .expect("stage removal");

        fs::write(&destination, b"new destination").expect("racing destination");
        let error = transaction
            .rollback()
            .expect_err("rollback must not replace a new destination");

        assert!(error.to_string().contains("rollback"));
        assert_eq!(
            fs::read(&destination).expect("preserved new destination"),
            b"new destination"
        );
        assert_eq!(
            fs::read(staging_root.join(".backup/mods/example.jar"))
                .expect("retained removal backup"),
            b"removed bytes"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn new_file_transaction_refuses_an_occupied_destination() {
        let root = root("new-file-occupied");
        fs::create_dir_all(root.join("mods")).expect("mods");
        fs::write(root.join("mods/example.jar"), b"user file").expect("existing");
        let staging = StagingGuard::create(&root, "stage").expect("stage");
        fs::create_dir_all(staging.path().join("mods")).expect("staged mods");
        fs::write(staging.path().join("mods/example.jar"), b"pack file").expect("staged");

        let relative_paths = vec!["mods/example.jar".to_string()];
        let inventory = ManagedContentInventory::capture(&root, &relative_paths)
            .expect("managed inventory");
        let result = FileTransaction::apply_new_with_inventory(
            &root,
            staging.transfer(),
            &relative_paths,
            &relative_paths,
            inventory,
        );

        assert!(result.is_err());
        assert_eq!(
            fs::read(root.join("mods/example.jar")).expect("preserved"),
            b"user file"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn failed_private_copy_does_not_remove_a_new_destination() {
        let root = root("failed-private-copy");
        let source = root.join("source.jar");
        let destination = root.join("destination.jar");
        fs::write(&source, b"managed bytes").expect("source");

        let result = promote_new_file_with_copy(&source, &destination, false, |_, private_copy| {
            fs::write(private_copy, b"partial private copy")?;
            fs::write(&destination, b"user replacement")?;
            Err(io::Error::other("simulated copy failure"))
        });

        assert!(result.is_err());
        assert_eq!(
            fs::read(&destination).expect("preserved replacement"),
            b"user replacement"
        );
        assert_eq!(
            fs::read(&source).expect("preserved source"),
            b"managed bytes"
        );
        let promotion_directories = fs::read_dir(&root)
            .expect("root entries")
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".axial-content-promotion-")
            })
            .count();
        assert_eq!(promotion_directories, 0);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn replacement_promotion_replaces_an_existing_file() {
        let root = root("replace-existing");
        let source = root.join("manifest.tmp");
        let destination = root.join("manifest.json");
        fs::write(&source, b"new").expect("source");
        fs::write(&destination, b"old").expect("destination");

        promote_replacement(&source, &destination).expect("promote");

        assert_eq!(fs::read(&destination).expect("destination"), b"new");
        assert!(!source.exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn replacement_fallback_replaces_a_windows_style_existing_destination() {
        let root = root("replace-existing-fallback");
        let source = root.join("manifest.tmp");
        let destination = root.join("manifest.json");
        fs::write(&source, b"new").expect("source");
        fs::write(&destination, b"old").expect("destination");

        promote_replacement_after_rename_failure(
            &source,
            &destination,
            std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "simulated Windows replacement failure",
            ),
        )
        .expect("fallback promotion");

        assert_eq!(fs::read(&destination).expect("destination"), b"new");
        assert!(!source.exists());
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn contained_path_rejects_symlinked_ancestors() {
        use std::os::unix::fs::symlink;

        let instance_root = root("symlink-ancestor");
        let outside = root("symlink-outside");
        symlink(&outside, instance_root.join("config")).expect("symlink");

        let result = contained_path(&instance_root, "config/options.txt");

        assert!(matches!(result, Err(ContentError::Invalid(_))));
        assert!(!outside.join("options.txt").exists());
        let _ = fs::remove_dir_all(instance_root);
        let _ = fs::remove_dir_all(outside);
    }
}
