use crate::error::{ContentError, ContentResult};
use crate::model::ContentKind;
use axial_minecraft::portable_path::{
    PortableFileName, PortablePathKey, PortableRelativePath, managed_content_name_is_reserved,
    managed_content_name_key,
};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
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
            )?
            else {
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
            let directory = directory_index(&parent_path, &mut directory_cache, &mut scan_budget)?;
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
            let disabled = format!("{requested}.disabled");
            assert!(
                inventory
                    .require_exact_managed_file_variant_or_absent(requested, &disabled)
                    .is_err(),
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
            ManagedContentInventory::capture(&aliased, &["mods/example.jar".to_string()]).is_err()
        );
        let _ = fs::remove_dir_all(aliased);

        let unrelated = root("unrelated-invalid-name");
        fs::create_dir(unrelated.join("mods")).expect("mods");
        fs::write(unrelated.join("bad:name"), b"unrelated").expect("unrelated entry");
        ManagedContentInventory::capture(&unrelated, &["mods/example.jar".to_string()])
            .expect("unrelated unportable root entry is outside the touched key");
        let _ = fs::remove_dir_all(unrelated);
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
