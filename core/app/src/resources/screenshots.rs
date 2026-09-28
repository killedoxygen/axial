//! Screenshot inventory and byte-preserving operations inside an admitted instance.
//!
//! Transport authentication remains at the API boundary. Filesystem authority is
//! obtained from the registered instance, never from a name supplied by a client.

use crate::files::{PortableName, ScopedDirectory};
use serde::{Deserialize, Serialize};
use std::io;
use ts_rs::TS;

/// Matches the retained resource scanner's maximum number of directory entries.
pub const SCREENSHOT_SCAN_MAX_ENTRIES: usize = 50_000;
pub const SCREENSHOT_SCAN_MAX_BYTES: u64 = 1024 * 1024 * 1024 * 1024;
/// Original screenshot bytes are served without decoding or recompression.
pub const SCREENSHOT_FILE_MAX_BYTES: u64 = 32 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct InstanceScreenshotInfo {
    pub name: String,
    pub size: u64,
    /// RFC 3339 UTC, or an empty string when the filesystem has no timestamp.
    pub modified_at: String,
}

#[derive(Debug, Deserialize)]
pub struct RenameScreenshotRequest {
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct RenameScreenshotResponse {
    pub status: &'static str,
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct DeleteScreenshotResponse {
    pub status: &'static str,
}

/// An adapter must send `content_type` and `X-Content-Type-Options: nosniff`.
#[derive(Debug)]
pub struct ScreenshotMedia {
    pub bytes: Vec<u8>,
    pub content_type: &'static str,
}

/// Closed, sanitized failures; no filesystem paths or native diagnostic text.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ScreenshotError {
    #[error("invalid screenshot filename")]
    InvalidName,
    #[error("screenshot file type cannot change")]
    FileTypeChange,
    #[error("screenshot not found")]
    NotFound,
    #[error("screenshot already exists")]
    AlreadyExists,
    #[error("screenshot changed; refresh and try again")]
    Changed,
    #[error("a screenshot update needs to finish before another change")]
    Pending,
    #[error("screenshot file is too large")]
    TooLarge,
    #[error("instance resources exceed safe scan limits")]
    ScanLimit,
    #[error("instance resources contain unsupported filesystem entries")]
    UnsupportedEntry,
    #[error("Could not read screenshot files. Check instance folder permissions and try again.")]
    Read,
    #[error("Could not update screenshot files. Check instance folder permissions and try again.")]
    Write,
}

impl ScreenshotError {
    pub fn status_code(self) -> u16 {
        match self {
            Self::InvalidName | Self::FileTypeChange => 400,
            Self::NotFound => 404,
            Self::AlreadyExists | Self::Changed | Self::Pending => 409,
            Self::TooLarge | Self::ScanLimit => 413,
            Self::UnsupportedEntry => 422,
            Self::Read | Self::Write => 500,
        }
    }
}

// The caller first validates the complete portable leaf name. MIME selection
// only examines the supported suffix and does not decode user content.
fn content_type_for_valid_name(name: &str) -> Option<&'static str> {
    let extension = name.rsplit_once('.')?.1;
    if extension.eq_ignore_ascii_case("png") {
        Some("image/png")
    } else if extension.eq_ignore_ascii_case("jpg") || extension.eq_ignore_ascii_case("jpeg") {
        Some("image/jpeg")
    } else if extension.eq_ignore_ascii_case("webp") {
        Some("image/webp")
    } else {
        None
    }
}

pub(crate) fn screenshot_name(name: &str) -> Result<PortableName, ScreenshotError> {
    let name = PortableName::new_exact(name).map_err(|_| ScreenshotError::InvalidName)?;
    content_type_for_valid_name(name.as_str()).ok_or(ScreenshotError::InvalidName)?;
    Ok(name)
}

pub(crate) fn list_screenshots(
    game: &ScopedDirectory,
) -> Result<Vec<InstanceScreenshotInfo>, ScreenshotError> {
    let directory =
        match game.open_directory(&PortableName::new_exact("screenshots").expect("fixed name")) {
            Ok(directory) => directory,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(_) => return Err(ScreenshotError::Read),
        };
    let listing = directory
        .entries(SCREENSHOT_SCAN_MAX_ENTRIES)
        .map_err(|_| ScreenshotError::Read)?;
    if listing.state() != axial_fs::DirectoryListingState::Complete {
        return Err(ScreenshotError::ScanLimit);
    }
    let mut total = 0_u64;
    let mut names = std::collections::HashSet::new();
    let mut result = Vec::new();
    for entry in listing.entries() {
        if matches!(
            entry.kind(),
            axial_fs::EntryKind::Link | axial_fs::EntryKind::Other
        ) {
            return Err(ScreenshotError::UnsupportedEntry);
        }
        if entry.kind() != axial_fs::EntryKind::File {
            continue;
        }
        let Some(name) = entry
            .utf8_name()
            .and_then(|name| screenshot_name(name).ok())
        else {
            continue;
        };
        if !names.insert(name.key()) {
            return Err(ScreenshotError::UnsupportedEntry);
        }
        let file = directory
            .open_file(&name)
            .map_err(|_| ScreenshotError::Changed)?;
        let revision = file.revision().map_err(|_| ScreenshotError::Changed)?;
        total = total
            .checked_add(revision.size())
            .filter(|total| *total <= SCREENSHOT_SCAN_MAX_BYTES)
            .ok_or(ScreenshotError::ScanLimit)?;
        result.push(InstanceScreenshotInfo {
            name: name.as_str().into(),
            size: revision.size(),
            modified_at: revision
                .modified_at_ns()
                .ok()
                .map(super::timestamp)
                .unwrap_or_default(),
        });
    }
    result.sort_by(|a, b| {
        b.modified_at
            .cmp(&a.modified_at)
            .then_with(|| a.name.cmp(&b.name))
    });
    Ok(result)
}

pub(crate) fn screenshot_media(
    game: &ScopedDirectory,
    name: &str,
) -> Result<ScreenshotMedia, ScreenshotError> {
    let name = screenshot_name(name)?;
    let path = crate::files::ScopedPath::new_exact(&format!("screenshots/{}", name.as_str()))
        .map_err(|_| ScreenshotError::InvalidName)?;
    let bytes = game
        .read_bounded(&path, SCREENSHOT_FILE_MAX_BYTES)
        .map_err(|error| match error.kind() {
            io::ErrorKind::NotFound => ScreenshotError::NotFound,
            io::ErrorKind::InvalidData => ScreenshotError::TooLarge,
            _ => ScreenshotError::Read,
        })?;
    Ok(ScreenshotMedia {
        bytes,
        content_type: content_type_for_valid_name(name.as_str()).expect("validated suffix"),
    })
}

pub(crate) fn validate_rename(
    from: &str,
    to: &str,
) -> Result<(PortableName, PortableName), ScreenshotError> {
    let from = screenshot_name(from)?;
    let to = screenshot_name(to)?;
    if content_type_for_valid_name(from.as_str()) != content_type_for_valid_name(to.as_str()) {
        return Err(ScreenshotError::FileTypeChange);
    }
    Ok((from, to))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supported_extensions_keep_the_existing_image_content_types() {
        for (name, mime) in [
            ("shot.png", "image/png"),
            ("shot.PNG", "image/png"),
            ("shot.JPG", "image/jpeg"),
            ("shot.jpeg", "image/jpeg"),
            ("shot.webp", "image/webp"),
        ] {
            assert_eq!(content_type_for_valid_name(name), Some(mime));
        }
        for name in ["shot.gif", "notes.txt", "png", "shot.png.exe"] {
            assert_eq!(content_type_for_valid_name(name), None);
        }
    }

    #[test]
    fn inventory_preserves_existing_wire_fields_and_timestamp_format() {
        let row = InstanceScreenshotInfo {
            name: "castle.png".into(),
            size: 124,
            modified_at: "2026-05-31T12:00:00+00:00".into(),
        };
        assert_eq!(
            serde_json::to_value(row).unwrap(),
            serde_json::json!({
                "name": "castle.png", "size": 124,
                "modified_at": "2026-05-31T12:00:00+00:00"
            })
        );
        assert_eq!(
            serde_json::to_value(RenameScreenshotResponse {
                status: "ok",
                name: "renamed.png".into()
            })
            .unwrap(),
            serde_json::json!({"status":"ok", "name":"renamed.png"})
        );
    }

    #[test]
    fn bounded_failures_distinguish_conflict_missing_input_and_io() {
        assert_eq!(ScreenshotError::NotFound.status_code(), 404);
        assert_eq!(ScreenshotError::AlreadyExists.status_code(), 409);
        assert_eq!(ScreenshotError::Changed.status_code(), 409);
        assert_eq!(ScreenshotError::InvalidName.status_code(), 400);
        assert_eq!(ScreenshotError::TooLarge.status_code(), 413);
        assert_eq!(ScreenshotError::UnsupportedEntry.status_code(), 422);
        assert_eq!(ScreenshotError::Read.status_code(), 500);
        assert_eq!(ScreenshotError::Write.status_code(), 500);
    }
}
