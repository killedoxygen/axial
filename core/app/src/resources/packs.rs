//! Resource pack and shader pack inventory below a scoped game root.
//!
//! Minecraft and shader loaders choose active packs in game; this listing only
//! describes what is installed.

use super::metadata::{
    ArchiveKind, ContentFileMetadata, ICON_MAX_BYTES, ReadBudget, archive_icon, archive_metadata,
    is_png, pack_directory_metadata,
};
use super::{ResourceError, timestamp};
use crate::content::model::ContentKind;
use crate::files::{
    PortableName, ScopedDirectory, ScopedPath, portable::managed_content_name_is_reserved,
};
use serde::Serialize;
use std::io;
use ts_rs::TS;

pub const PACK_SCAN_MAX_ENTRIES: usize = 10_000;
const MCMETA_MAX_BYTES: u64 = 256 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PackFolder {
    ResourcePacks,
    ShaderPacks,
}

impl PackFolder {
    pub fn directory(self) -> &'static str {
        match self {
            Self::ResourcePacks => "resourcepacks",
            Self::ShaderPacks => "shaderpacks",
        }
    }
    pub(crate) fn content_kind(self) -> ContentKind {
        match self {
            Self::ResourcePacks => ContentKind::ResourcePack,
            Self::ShaderPacks => ContentKind::ShaderPack,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, TS)]
pub struct InstancePackInfo {
    pub name: String,
    /// An unpacked pack folder rather than a `.zip` archive.
    pub directory: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub size: Option<u64>,
    /// RFC 3339 UTC. A folder reports its `pack.png`, the one file served from it. Empty without
    /// that file or when the filesystem supplies no timestamp.
    pub modified_at: String,
    /// Disabled archives carry a `.disabled` suffix and are hidden from the game.
    pub enabled: bool,
    pub metadata: ContentFileMetadata,
}

pub(crate) fn list_packs(
    game: &ScopedDirectory,
    folder: PackFolder,
) -> Result<Vec<InstancePackInfo>, ResourceError> {
    let parent = match open_folder(game, folder) {
        Ok(parent) => parent,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(_) => return Err(ResourceError::Files),
    };
    let listing = parent
        .entries(PACK_SCAN_MAX_ENTRIES)
        .map_err(|_| ResourceError::Files)?;
    if listing.state() != axial_fs::DirectoryListingState::Complete {
        return Err(ResourceError::Limit);
    }
    let mut budget = ReadBudget::listing();
    let mut packs = Vec::new();
    for entry in listing.entries() {
        let Some(name) = entry
            .utf8_name()
            .and_then(|name| PortableName::new_exact(name).ok())
            .filter(|name| !managed_content_name_is_reserved(name))
        else {
            continue;
        };
        match entry.kind() {
            axial_fs::EntryKind::Directory => {
                let Ok(directory) = parent.open_directory(&name) else {
                    continue;
                };
                let (metadata, modified_at) = match folder {
                    PackFolder::ResourcePacks => directory_metadata(&directory, &mut budget),
                    PackFolder::ShaderPacks => Default::default(),
                };
                packs.push(InstancePackInfo {
                    name: name.as_str().into(),
                    directory: true,
                    size: None,
                    modified_at,
                    enabled: true,
                    metadata,
                });
            }
            axial_fs::EntryKind::File => {
                let Some(enabled) = archive_state(&name) else {
                    continue;
                };
                let file = parent.open_file(&name).map_err(|_| ResourceError::Files)?;
                let revision = file.revision().map_err(|_| ResourceError::Files)?;
                let metadata = match folder {
                    PackFolder::ResourcePacks => {
                        archive_metadata(&file, ArchiveKind::ResourcePack, &mut budget)
                    }
                    PackFolder::ShaderPacks => ContentFileMetadata::default(),
                };
                packs.push(InstancePackInfo {
                    name: name.as_str().into(),
                    directory: false,
                    size: Some(revision.size()),
                    modified_at: revision
                        .modified_at_ns()
                        .ok()
                        .map(timestamp)
                        .unwrap_or_default(),
                    enabled,
                    metadata,
                });
            }
            // Links and special entries are not followed or described.
            axial_fs::EntryKind::Link | axial_fs::EntryKind::Other => {}
        }
    }
    packs.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(packs)
}

/// Some(enabled) for a `.zip` or `.zip.disabled` archive name.
fn archive_state(name: &PortableName) -> Option<bool> {
    let key = name.key();
    if key.as_str().ends_with(".zip") {
        Some(true)
    } else if key.as_str().ends_with(".zip.disabled") {
        Some(false)
    } else {
        None
    }
}

/// Validate a pack archive selector for deletion.
pub(crate) fn archive_name(name: &str) -> Result<PortableName, ResourceError> {
    let name = PortableName::new_exact(name).map_err(|_| ResourceError::InvalidName)?;
    if managed_content_name_is_reserved(&name) || archive_state(&name).is_none() {
        return Err(ResourceError::InvalidName);
    }
    Ok(name)
}

pub(crate) fn pack_icon(game: &ScopedDirectory, name: &str) -> Result<Vec<u8>, ResourceError> {
    let name = PortableName::new_exact(name).map_err(|_| ResourceError::InvalidName)?;
    if managed_content_name_is_reserved(&name) {
        return Err(ResourceError::InvalidName);
    }
    let parent =
        open_folder(game, PackFolder::ResourcePacks).map_err(|_| ResourceError::NotFound)?;
    let icon = if archive_state(&name).is_some() {
        let file = parent
            .open_file(&name)
            .map_err(|_| ResourceError::NotFound)?;
        archive_icon(&file, ArchiveKind::ResourcePack)
    } else {
        let directory = parent
            .open_directory(&name)
            .map_err(|_| ResourceError::NotFound)?;
        directory
            .read_bounded(&fixed_path("pack.png"), ICON_MAX_BYTES)
            .ok()
            .filter(|bytes| is_png(bytes))
    };
    icon.ok_or(ResourceError::NotFound)
}

/// What a pack folder says about itself, and when its icon last changed.
fn directory_metadata(
    directory: &ScopedDirectory,
    budget: &mut ReadBudget,
) -> (ContentFileMetadata, String) {
    let mcmeta = budget
        .take(MCMETA_MAX_BYTES)
        .then(|| {
            directory
                .read_bounded(&fixed_path("pack.mcmeta"), MCMETA_MAX_BYTES)
                .ok()
        })
        .flatten();
    let icon = directory
        .open_file(&PortableName::new_exact("pack.png").expect("fixed portable name"))
        .ok();
    let modified_at = icon
        .as_ref()
        .and_then(|file| file.revision().ok())
        .and_then(|revision| revision.modified_at_ns().ok())
        .map(timestamp)
        .unwrap_or_default();
    (
        pack_directory_metadata(mcmeta.as_deref(), icon.is_some()),
        modified_at,
    )
}

fn open_folder(game: &ScopedDirectory, folder: PackFolder) -> io::Result<ScopedDirectory> {
    game.open_directory(&PortableName::new_exact(folder.directory()).expect("fixed portable name"))
}

fn fixed_path(path: &str) -> ScopedPath {
    ScopedPath::new_exact(path).expect("fixed portable path")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn archive_selectors_admit_only_pack_archives() {
        for name in ["Faithful.zip", "Faithful.ZIP", "old.zip.disabled"] {
            assert!(archive_name(name).is_ok(), "{name}");
        }
        for name in [
            "",
            "..",
            "pack",
            "pack.zip.txt",
            "nested/pack.zip",
            "CON.zip",
        ] {
            assert!(
                matches!(archive_name(name), Err(ResourceError::InvalidName)),
                "{name}"
            );
        }
    }

    #[test]
    fn listing_describes_archives_and_folders_without_following_other_entries() {
        use crate::library::{LibraryLifecycle, LibraryOpenOutcome};
        let directory = tempfile::tempdir().unwrap();
        let physical = directory.path().canonicalize().unwrap();
        let packs = physical.join("resourcepacks");
        std::fs::create_dir(&packs).unwrap();
        std::fs::write(
            packs.join("Faithful.zip"),
            super::super::metadata::tests::jar(&[
                ("pack.mcmeta", br#"{"pack":{"description":"32x"}}"#),
                ("pack.png", b"\x89PNG\r\n\x1a\nicon"),
            ]),
        )
        .unwrap();
        std::fs::write(packs.join("Old.zip.disabled"), b"broken").unwrap();
        std::fs::write(packs.join("notes.txt"), b"user notes").unwrap();
        std::fs::create_dir(packs.join("Unpacked")).unwrap();
        std::fs::write(
            packs.join("Unpacked/pack.mcmeta"),
            br#"{"pack":{"description":"Loose"}}"#,
        )
        .unwrap();
        std::fs::create_dir(packs.join("Painted")).unwrap();
        std::fs::write(packs.join("Painted/pack.png"), b"\x89PNG\r\n\x1a\nloose").unwrap();
        let shaders = physical.join("shaderpacks");
        std::fs::create_dir(&shaders).unwrap();
        std::fs::write(shaders.join("Complementary.zip"), b"zip").unwrap();
        std::fs::write(shaders.join("Complementary.zip.txt"), b"options").unwrap();

        let owner = match LibraryLifecycle::open(&physical) {
            LibraryOpenOutcome::Ready(owner) => owner,
            other => panic!("fixture acquisition failed: {other:?}"),
        };
        let root = owner.admit().unwrap().files().unwrap();
        let listed = list_packs(&root, PackFolder::ResourcePacks).unwrap();
        assert_eq!(
            listed
                .iter()
                .map(|pack| (
                    pack.name.as_str(),
                    pack.directory,
                    pack.enabled,
                    pack.metadata.description.as_deref(),
                    pack.metadata.icon
                ))
                .collect::<Vec<_>>(),
            vec![
                ("Faithful.zip", false, true, Some("32x"), true),
                ("Old.zip.disabled", false, false, None, false),
                ("Painted", true, true, None, true),
                ("Unpacked", true, true, Some("Loose"), false),
            ]
        );
        // A folder's icon address needs a revision; a folder without one has none.
        assert!(chrono::DateTime::parse_from_rfc3339(&listed[2].modified_at).is_ok());
        assert_eq!(listed[3].modified_at, "");
        assert_eq!(
            pack_icon(&root, "Painted").unwrap(),
            b"\x89PNG\r\n\x1a\nloose"
        );
        assert_eq!(
            pack_icon(&root, "Faithful.zip").unwrap(),
            b"\x89PNG\r\n\x1a\nicon"
        );
        assert!(matches!(
            pack_icon(&root, "Unpacked"),
            Err(ResourceError::NotFound)
        ));
        let shaders = list_packs(&root, PackFolder::ShaderPacks).unwrap();
        assert_eq!(
            shaders
                .iter()
                .map(|pack| pack.name.as_str())
                .collect::<Vec<_>>(),
            ["Complementary.zip"]
        );
        drop(root);
        owner.close_admission();
        assert!(matches!(
            owner.revoke_application_root().unwrap(),
            axial_fs::RootRevokeOutcome::Revoked
        ));
        assert_eq!(
            std::fs::read(packs.join("notes.txt")).unwrap(),
            b"user notes"
        );
    }
}
