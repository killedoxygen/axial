//! World saves are user data. Names select children of an admitted game directory;
//! they never admit a caller-authored filesystem path.

use crate::files::PortableFileName;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use ts_rs::TS;

pub const WORLD_SCAN_MAX_DEPTH: usize = 32;
pub const WORLD_SCAN_MAX_ENTRIES: usize = 50_000;
pub const WORLD_SCAN_MAX_BYTES: u64 = 1024 * 1024 * 1024 * 1024;
pub const WORLD_BACKUP_MAX_DEPTH: usize = 64;
pub const WORLD_BACKUP_MAX_ENTRIES: usize = 100_000;
pub const WORLD_BACKUP_MAX_BYTES: u64 = 50 * 1024 * 1024 * 1024;
pub const WORLD_ICON_MAX_BYTES: u64 = 1024 * 1024;
pub(super) const BACKUP_NAME_ATTEMPTS: usize = 100;

/// Preserves the existing resource inventory wire shape.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, TS)]
pub struct InstanceWorldInfo {
    pub name: String,
    pub size: u64,
    pub modified_at: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RenameWorldRequest {
    pub name: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct WorldBackupResponse {
    pub status: String,
    pub backup: String,
    pub location: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct WorldCommandResponse {
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// Bounded icon bytes for the media adapter. This is not a filesystem projection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorldIcon {
    pub bytes: Vec<u8>,
    pub content_type: &'static str,
}

/// Public messages intentionally omit physical paths and provider diagnostics.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum WorldError {
    #[error("invalid world name")]
    InvalidName,
    #[error("world not found")]
    NotFound,
    #[error("world already exists")]
    AlreadyExists,
    #[error("World changes are unavailable while this instance is in use.")]
    Busy,
    #[error("World resources exceed safe filesystem limits.")]
    Capacity,
    #[error("World resources contain unsupported filesystem entries.")]
    UnsupportedEntry,
    #[error("World files changed during this operation. Refresh and try again.")]
    Changed,
    #[error("Could not complete the world operation. Check folder permissions and try again.")]
    Io,
    #[error("World operation requires settlement. Existing files have been preserved.")]
    SettlementRequired,
    #[error("world icon not found")]
    IconNotFound,
}

impl WorldError {
    pub fn status_code(self) -> u16 {
        match self {
            Self::InvalidName => 400,
            Self::NotFound | Self::IconNotFound => 404,
            Self::AlreadyExists | Self::Busy | Self::Changed | Self::SettlementRequired => 409,
            Self::Capacity => 413,
            Self::UnsupportedEntry => 422,
            Self::Io => 500,
        }
    }
}

pub(crate) fn world_name(value: &str) -> Result<PortableFileName, WorldError> {
    PortableFileName::new_exact(value).map_err(|_| WorldError::InvalidName)
}

// The original stem may fill either portable filename limit. Keep the digest
// outside the truncated stem so distinct long world names stay distinguishable.
pub(crate) fn backup_name(
    world: &PortableFileName,
    timestamp: &str,
    attempt: usize,
) -> PortableFileName {
    let digest = Sha256::digest(world.as_str().as_bytes());
    let digest = digest[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let ordinal = if attempt == 1 {
        String::new()
    } else {
        format!("-{attempt}")
    };
    let suffix = format!("-{timestamp}-{digest}{ordinal}");
    let mut stem = String::new();
    let mut utf16_units = 0;
    for character in world.as_str().chars() {
        if stem.len() + character.len_utf8() + suffix.len() > 255
            || utf16_units + character.len_utf16() + suffix.encode_utf16().count() > 255
        {
            break;
        }
        stem.push(character);
        utf16_units += character.len_utf16();
    }
    PortableFileName::new_exact(&format!("{stem}{suffix}"))
        .expect("validated world name and generated suffix remain portable")
}

pub(crate) fn list_worlds(
    game: &crate::files::ScopedDirectory,
) -> Result<Vec<InstanceWorldInfo>, WorldError> {
    let saves =
        match game.open_directory(&PortableFileName::new_exact("saves").expect("fixed name")) {
            Ok(saves) => saves,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(_) => return Err(WorldError::Io),
        };
    let listing = saves
        .entries(WORLD_SCAN_MAX_ENTRIES)
        .map_err(|_| WorldError::Io)?;
    if listing.state() != axial_fs::DirectoryListingState::Complete {
        return Err(WorldError::Capacity);
    }
    let mut count = listing.entries().len();
    let mut total = 0_u64;
    let mut names = std::collections::HashSet::new();
    let mut result = Vec::new();
    for entry in listing.entries() {
        if matches!(
            entry.kind(),
            axial_fs::EntryKind::Link | axial_fs::EntryKind::Other
        ) {
            return Err(WorldError::UnsupportedEntry);
        }
        if entry.kind() != axial_fs::EntryKind::Directory {
            continue;
        }
        let name = world_name(entry.utf8_name().ok_or(WorldError::UnsupportedEntry)?)?;
        if !names.insert(name.key()) {
            return Err(WorldError::UnsupportedEntry);
        }
        let directory = saves
            .open_directory(&name)
            .map_err(|_| WorldError::Changed)?;
        let before = total;
        let modified = scan(&directory, 0, &mut count, &mut total)?;
        result.push(InstanceWorldInfo {
            name: name.as_str().into(),
            size: total - before,
            modified_at: modified.map(super::timestamp).unwrap_or_default(),
        });
    }
    result.sort_by(|a, b| {
        a.name
            .to_lowercase()
            .cmp(&b.name.to_lowercase())
            .then_with(|| a.name.cmp(&b.name))
    });
    Ok(result)
}

fn scan(
    directory: &crate::files::ScopedDirectory,
    depth: usize,
    count: &mut usize,
    bytes: &mut u64,
) -> Result<Option<u64>, WorldError> {
    if depth >= WORLD_SCAN_MAX_DEPTH {
        return Err(WorldError::Capacity);
    }
    let revision = directory.revision().map_err(|_| WorldError::Changed)?;
    let listing = directory
        .entries(WORLD_SCAN_MAX_ENTRIES.saturating_sub(*count))
        .map_err(|_| WorldError::Io)?;
    if listing.state() != axial_fs::DirectoryListingState::Complete {
        return Err(WorldError::Capacity);
    }
    *count = count
        .checked_add(listing.entries().len())
        .filter(|count| *count <= WORLD_SCAN_MAX_ENTRIES)
        .ok_or(WorldError::Capacity)?;
    let mut latest = None;
    let mut names = std::collections::HashSet::new();
    for entry in listing.entries() {
        let name = world_name(entry.utf8_name().ok_or(WorldError::UnsupportedEntry)?)?;
        if !names.insert(name.key()) {
            return Err(WorldError::UnsupportedEntry);
        }
        let modified = match entry.kind() {
            axial_fs::EntryKind::Directory => scan(
                &directory
                    .open_directory(&name)
                    .map_err(|_| WorldError::Changed)?,
                depth + 1,
                count,
                bytes,
            )?,
            axial_fs::EntryKind::File => {
                let revision = directory
                    .open_file(&name)
                    .map_err(|_| WorldError::Changed)?
                    .revision()
                    .map_err(|_| WorldError::Changed)?;
                *bytes = bytes
                    .checked_add(revision.size())
                    .filter(|bytes| *bytes <= WORLD_SCAN_MAX_BYTES)
                    .ok_or(WorldError::Capacity)?;
                revision.modified_at_ns().ok()
            }
            _ => return Err(WorldError::UnsupportedEntry),
        };
        latest = latest.max(modified);
    }
    directory
        .validate_revision(&revision)
        .map_err(|_| WorldError::Changed)?;
    Ok(latest)
}

pub(crate) fn world_icon(
    game: &crate::files::ScopedDirectory,
    name: &str,
) -> Result<WorldIcon, WorldError> {
    let name = world_name(name)?;
    let path = crate::files::ScopedPath::new_exact(&format!("saves/{}/icon.png", name.as_str()))
        .map_err(|_| WorldError::InvalidName)?;
    let bytes = game
        .read_bounded(&path, WORLD_ICON_MAX_BYTES)
        .map_err(|_| WorldError::IconNotFound)?;
    if !bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Err(WorldError::IconNotFound);
    }
    Ok(WorldIcon {
        bytes,
        content_type: "image/png",
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_names_keep_distinct_bounded_backup_names() {
        let first = world_name(&format!("{}a", "w".repeat(254))).unwrap();
        let second = world_name(&format!("{}b", "w".repeat(254))).unwrap();
        assert_ne!(
            backup_name(&first, "20260908T123456Z", 1),
            backup_name(&second, "20260908T123456Z", 1)
        );
        for name in [first, world_name(&"😀".repeat(63)).unwrap()] {
            for attempt in 1..=BACKUP_NAME_ATTEMPTS {
                let backup = backup_name(&name, "20260908T123456Z", attempt);
                assert!(backup.as_str().len() <= 255);
                assert!(backup.as_str().encode_utf16().count() <= 255);
                if attempt > 1 {
                    assert!(backup.as_str().ends_with(&format!("-{attempt}")));
                }
            }
        }
    }

    #[test]
    fn world_names_keep_exact_portable_spelling() {
        for name in ["World", "My World", ".hidden", " World", "café"] {
            assert!(world_name(name).is_ok(), "{name:?}");
        }
        for name in [
            "",
            "   ",
            ".",
            "..",
            "../World",
            "nested/World",
            "nested\\World",
            "cafe\u{301}",
            "World ",
            "CON",
            "bad\nworld",
        ] {
            assert_eq!(world_name(name), Err(WorldError::InvalidName), "{name:?}");
        }
    }

    #[test]
    fn delete_response_omits_rename_only_field() {
        let result = WorldCommandResponse {
            status: "ok".into(),
            name: None,
        };
        assert_eq!(
            serde_json::to_value(result).unwrap(),
            serde_json::json!({"status":"ok"})
        );
    }
}
