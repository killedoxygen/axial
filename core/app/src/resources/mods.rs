//! Mod-file resources for registered instances.
//!
//! Resource mutations are executed by the content transaction owner so that a
//! rename and its provenance update share one settlement obligation.

use crate::files::ScopedDirectory;
use crate::files::portable::{
    PortableFileName, managed_content_name_is_reserved, managed_content_name_key,
};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, io};
use ts_rs::TS;

pub const MOD_SCAN_MAX_ENTRIES: usize = 50_000;
pub const MOD_SCAN_MAX_BYTES: u64 = 1024 * 1024 * 1024 * 1024;

/// The retained resource-list wire shape.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct InstanceModInfo {
    pub name: String,
    pub size: u64,
    /// RFC 3339 UTC, or empty when the filesystem supplies no timestamp.
    pub modified_at: String,
    pub enabled: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
pub struct UpdateModRequest {
    pub enabled: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct UpdateModResponse {
    pub status: &'static str,
    pub name: String,
    pub enabled: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct DeleteModResponse {
    pub status: &'static str,
}

/// Public failures contain no native paths or provider diagnostic text.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ModFilesError {
    #[error("invalid mod filename")]
    InvalidName,
    #[error("instance not found")]
    InstanceNotFound,
    #[error("mod not found")]
    NotFound,
    #[error("another launch or content operation is already using this instance")]
    Busy,
    #[error("application shutdown or update is in progress; try the mod change again")]
    Unavailable,
    #[error("the mod toggle destination is already occupied")]
    AlreadyExists,
    #[error("mod files changed; refresh and try again")]
    Changed,
    #[error("managed mods must be removed through content operations")]
    Managed,
    #[error("instance resources exceed safe scan limits")]
    ScanLimit,
    #[error("instance resources contain unsupported filesystem entries")]
    UnsupportedEntry,
    #[error("mod changes require settlement before another operation can proceed")]
    SettlementRequired,
    #[error("could not read mod files; check instance folder permissions and try again")]
    Read,
    #[error("could not update mod files; check instance folder permissions and try again")]
    Write,
}

impl ModFilesError {
    pub fn status_code(self) -> u16 {
        match self {
            Self::InvalidName => 400,
            Self::InstanceNotFound | Self::NotFound => 404,
            Self::Busy
            | Self::AlreadyExists
            | Self::Changed
            | Self::Managed
            | Self::SettlementRequired => 409,
            Self::ScanLimit => 413,
            Self::UnsupportedEntry => 422,
            Self::Unavailable => 503,
            Self::Read | Self::Write => 500,
        }
    }
}

/// Lists only the fixed `mods` resource directory below a scoped game root.
/// The caller retains its registered-instance admission during this read.
pub(crate) fn list_mods(game: &ScopedDirectory) -> Result<Vec<InstanceModInfo>, ModFilesError> {
    let mods_name = PortableFileName::new_exact("mods").expect("fixed portable name");
    let mods = match game.open_directory(&mods_name) {
        Ok(mods) => mods,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(read_error(error)),
    };
    let listing = mods.entries(MOD_SCAN_MAX_ENTRIES).map_err(read_error)?;
    if listing.state() != axial_fs::DirectoryListingState::Complete {
        return Err(ModFilesError::ScanLimit);
    }
    let mut entries = Vec::new();
    for entry in listing.entries() {
        match entry.kind() {
            axial_fs::EntryKind::Directory => continue,
            axial_fs::EntryKind::Link | axial_fs::EntryKind::Other => {
                return Err(ModFilesError::UnsupportedEntry);
            }
            axial_fs::EntryKind::File => {}
        }
        let Some(name) = entry.utf8_name().and_then(listed_mod_name) else {
            continue;
        };
        let file = mods.open_file(&name).map_err(read_error)?;
        let revision = file.revision().map_err(read_error)?;
        entries.push(InstanceModInfo {
            name: name.as_str().to_string(),
            size: revision.size(),
            modified_at: revision
                .modified_at_ns()
                .ok()
                .map(format_timestamp)
                .unwrap_or_default(),
            enabled: false,
        });
    }
    collect_mods(entries)
}

fn read_error(error: io::Error) -> ModFilesError {
    match error.kind() {
        io::ErrorKind::NotFound => ModFilesError::Changed,
        io::ErrorKind::InvalidData | io::ErrorKind::InvalidInput | io::ErrorKind::NotADirectory => {
            ModFilesError::UnsupportedEntry
        }
        _ => ModFilesError::Read,
    }
}

fn format_timestamp(nanoseconds: u64) -> String {
    let seconds = i64::try_from(nanoseconds / 1_000_000_000).ok();
    seconds
        .and_then(|seconds| {
            chrono::DateTime::<chrono::Utc>::from_timestamp(
                seconds,
                (nanoseconds % 1_000_000_000) as u32,
            )
        })
        .map(|timestamp| timestamp.to_rfc3339())
        .unwrap_or_default()
}

/// Validate the client's selector. A valid selector still grants no file access.
pub fn validate_mod_filename(name: &str) -> Result<(), ModFilesError> {
    let portable = listed_mod_name(name).ok_or(ModFilesError::InvalidName)?;
    if managed_content_name_is_reserved(&portable) {
        return Err(ModFilesError::InvalidName);
    }
    Ok(())
}

fn listed_mod_name(name: &str) -> Option<PortableFileName> {
    let portable = PortableFileName::new_exact(name).ok()?;
    let key = portable.key();
    if key.as_str().ends_with(".jar") || key.as_str().ends_with(".jar.disabled") {
        Some(portable)
    } else {
        None
    }
}

/// Shares the enabled/disabled collision namespace used by content ownership.
fn collect_mods(
    entries: impl IntoIterator<Item = InstanceModInfo>,
) -> Result<Vec<InstanceModInfo>, ModFilesError> {
    let mut mods = BTreeMap::new();
    let mut total_bytes = 0_u64;
    for entry in entries {
        let Some(name) = listed_mod_name(&entry.name) else {
            continue;
        };
        total_bytes = total_bytes
            .checked_add(entry.size)
            .filter(|size| *size <= MOD_SCAN_MAX_BYTES)
            .ok_or(ModFilesError::ScanLimit)?;
        let enabled = name.key().as_str().ends_with(".jar");
        if mods
            .insert(
                managed_content_name_key(&name),
                InstanceModInfo { enabled, ..entry },
            )
            .is_some()
        {
            return Err(ModFilesError::UnsupportedEntry);
        }
        if mods.len() > MOD_SCAN_MAX_ENTRIES {
            return Err(ModFilesError::ScanLimit);
        }
    }
    Ok(mods.into_values().collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_scope(test: impl FnOnce(&std::path::Path, &ScopedDirectory)) {
        use crate::library::{LibraryLifecycle, LibraryOpenOutcome};
        let directory = tempfile::tempdir().unwrap();
        let physical = directory.path().canonicalize().unwrap();
        let owner = match LibraryLifecycle::open(&physical) {
            LibraryOpenOutcome::Ready(owner) => owner,
            LibraryOpenOutcome::NoEffect(error) => panic!("fixture acquisition failed: {error}"),
            LibraryOpenOutcome::Unresolved(obligation) => {
                assert!(obligation.acknowledge_preserved().is_ok());
                panic!("fixture acquisition was unresolved");
            }
        };
        let root = owner.admit().unwrap().files().unwrap();
        test(&physical, &root);
        drop(root);
        owner.close_admission();
        assert!(matches!(
            owner.revoke_application_root().unwrap(),
            axial_fs::RootRevokeOutcome::Revoked
        ));
    }

    fn entry(name: &str, size: u64) -> InstanceModInfo {
        InstanceModInfo {
            name: name.to_string(),
            size,
            modified_at: "2026-09-08T12:34:56+00:00".to_string(),
            enabled: false,
        }
    }

    #[test]
    fn mod_selectors_retain_unicode_and_hidden_names_without_admitting_paths() {
        for name in [
            "sodium.jar",
            "Sodium.JAR",
            "sodium.jar.disabled",
            ".hidden.jar",
            " café.jar",
        ] {
            assert!(validate_mod_filename(name).is_ok(), "{name}");
        }
        for name in [
            "",
            "   ",
            ".",
            "..",
            "CON.jar",
            ".axial-pack-staging.jar",
            "cafe\u{301}.jar",
            "../mod.jar",
            "nested/mod.jar",
            "nested\\mod.jar",
            "bad\nmod.jar",
            "notes.txt",
            "mod.disabled",
        ] {
            assert_eq!(
                validate_mod_filename(name),
                Err(ModFilesError::InvalidName),
                "{name}"
            );
        }
    }

    #[test]
    fn listing_sorts_portable_names_and_derives_enabled_state_from_the_filename() {
        let mods = collect_mods([
            entry("z.jar.disabled", 9),
            entry("README.txt", 10),
            entry("B.JAR", 3),
            entry("a.jar", 0),
        ])
        .unwrap();
        assert_eq!(
            mods.iter()
                .map(|entry| (entry.name.as_str(), entry.enabled))
                .collect::<Vec<_>>(),
            vec![("a.jar", true), ("B.JAR", true), ("z.jar.disabled", false)]
        );
        assert_eq!(mods[0].size, 0);
    }

    #[test]
    fn listing_refuses_casefold_and_enabled_disabled_collisions() {
        for names in [
            ["Straße.jar", "STRASSE.JAR"],
            ["sodium.jar", "SODIUM.jar.disabled"],
        ] {
            assert_eq!(
                collect_mods(names.map(|name| entry(name, 1))),
                Err(ModFilesError::UnsupportedEntry)
            );
        }
    }

    #[test]
    fn listing_preserves_wire_shape_and_bounds_reported_bytes() {
        let mods = collect_mods([entry("example.jar", 3)]).unwrap();
        assert_eq!(
            serde_json::to_value(&mods).unwrap(),
            serde_json::json!([{
                "name": "example.jar", "size": 3,
                "modified_at": "2026-09-08T12:34:56+00:00", "enabled": true
            }])
        );
        assert_eq!(
            collect_mods([entry("a.jar", MOD_SCAN_MAX_BYTES), entry("b.jar", 1)]),
            Err(ModFilesError::ScanLimit)
        );
    }

    #[test]
    fn scoped_listing_reads_real_metadata_and_keeps_unrelated_entries_untouched() {
        with_scope(|path, root| {
            assert_eq!(list_mods(root).unwrap(), vec![]);
            let mods = path.join("mods");
            std::fs::create_dir(&mods).unwrap();
            std::fs::create_dir(mods.join("folder.jar")).unwrap();
            std::fs::write(mods.join("z.jar.disabled"), b"disabled").unwrap();
            std::fs::write(mods.join("A.JAR"), b"active").unwrap();
            std::fs::write(mods.join("README.txt"), b"user notes").unwrap();
            let entries = list_mods(root).unwrap();
            assert_eq!(
                entries
                    .iter()
                    .map(|entry| (entry.name.as_str(), entry.size, entry.enabled))
                    .collect::<Vec<_>>(),
                vec![("A.JAR", 6, true), ("z.jar.disabled", 8, false)]
            );
            assert!(
                entries
                    .iter()
                    .all(|entry| chrono::DateTime::parse_from_rfc3339(&entry.modified_at).is_ok())
            );
            assert_eq!(
                std::fs::read(mods.join("README.txt")).unwrap(),
                b"user notes"
            );
            assert!(mods.join("folder.jar").is_dir());
        });
    }

    #[test]
    fn scoped_listing_refuses_two_variants_in_one_mod_namespace() {
        with_scope(|path, root| {
            let mods = path.join("mods");
            std::fs::create_dir(&mods).unwrap();
            std::fs::write(mods.join("example.jar"), b"first").unwrap();
            std::fs::write(mods.join("example.jar.disabled"), b"second").unwrap();
            assert_eq!(list_mods(root), Err(ModFilesError::UnsupportedEntry));
            assert_eq!(std::fs::read(mods.join("example.jar")).unwrap(), b"first");
            assert_eq!(
                std::fs::read(mods.join("example.jar.disabled")).unwrap(),
                b"second"
            );
        });
    }

    #[cfg(unix)]
    #[test]
    fn scoped_listing_refuses_symlinks_without_reading_the_target() {
        with_scope(|path, root| {
            let outside = tempfile::tempdir().unwrap();
            let secret = outside.path().join("outside.jar");
            std::fs::write(&secret, b"outside bytes").unwrap();
            let mods = path.join("mods");
            std::fs::create_dir(&mods).unwrap();
            std::os::unix::fs::symlink(&secret, mods.join("linked.jar")).unwrap();
            assert_eq!(list_mods(root), Err(ModFilesError::UnsupportedEntry));
            assert_eq!(std::fs::read(secret).unwrap(), b"outside bytes");
        });
    }
}
