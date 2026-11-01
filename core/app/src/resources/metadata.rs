//! Display metadata read from mod jars and pack archives.
//!
//! Reads are bounded per archive and per listing. Metadata is best-effort
//! display text: it never identifies content or grants file authority.

use crate::files::ScopedFile;
use axial_fs::{FileCapability, FileRevision};
use serde::Serialize;
use std::io::{self, BufReader, Read, Seek, SeekFrom};
use ts_rs::TS;

/// Bytes one archive may consume, including its central directory.
const ARCHIVE_READ_BYTES: u64 = 8 * 1024 * 1024;
/// Bytes one listing may consume across every archive it describes.
pub(crate) const LISTING_READ_BYTES: u64 = 96 * 1024 * 1024;
const METADATA_MAX_BYTES: u64 = 256 * 1024;
pub(crate) const ICON_MAX_BYTES: u64 = 512 * 1024;
const RANGE_BYTES: usize = 64 * 1024;
const TITLE_CHARS: usize = 96;
const VERSION_CHARS: usize = 64;
const AUTHOR_CHARS: usize = 64;
const AUTHORS: usize = 6;
const DESCRIPTION_CHARS: usize = 280;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, TS)]
pub struct ContentFileMetadata {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub version: Option<String>,
    pub authors: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub description: Option<String>,
    /// The icon route can serve a PNG for this file.
    pub icon: bool,
}

/// Remaining bytes for one listing. Exhaustion leaves later files undescribed.
pub(crate) struct ReadBudget {
    remaining: u64,
}

impl ReadBudget {
    pub(crate) fn listing() -> Self {
        Self {
            remaining: LISTING_READ_BYTES,
        }
    }
    /// Reserve bytes before reading them.
    pub(crate) fn take(&mut self, bytes: u64) -> bool {
        let reserved = self.remaining >= bytes;
        if reserved {
            self.remaining -= bytes;
        }
        reserved
    }
    #[cfg(test)]
    fn bytes(remaining: u64) -> Self {
        Self { remaining }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ArchiveKind {
    Mod,
    ResourcePack,
}

/// Describe an archive. Unreadable or unrecognised archives yield no metadata.
pub(crate) fn archive_metadata(
    file: &ScopedFile,
    kind: ArchiveKind,
    budget: &mut ReadBudget,
) -> ContentFileMetadata {
    let Ok(revision) = file.revision() else {
        return ContentFileMetadata::default();
    };
    // Reserve the archive's allowance up front and refund what it did not read.
    let allowance = budget.remaining.min(ARCHIVE_READ_BYTES);
    if allowance == 0 || !budget.take(allowance) {
        return ContentFileMetadata::default();
    }
    let reader = RangeReader::new(file.capability(), &revision, allowance);
    let Ok(mut archive) = zip::ZipArchive::new(BufReader::with_capacity(16 * 1024, reader)) else {
        return ContentFileMetadata::default();
    };
    let (mut metadata, icon) = match kind {
        ArchiveKind::Mod => mod_metadata(&mut archive),
        ArchiveKind::ResourcePack => {
            let mcmeta = entry_bytes(&mut archive, "pack.mcmeta", METADATA_MAX_BYTES);
            (
                mcmeta.as_deref().map(pack_metadata).unwrap_or_default(),
                Some("pack.png".to_string()),
            )
        }
    };
    metadata.icon = icon.is_some_and(|path| entry_exists(&mut archive, &path));
    budget.remaining += allowance - archive.into_inner().get_ref().consumed;
    metadata
}

/// The archive's icon bytes, when it names a bounded PNG.
pub(crate) fn archive_icon(file: &ScopedFile, kind: ArchiveKind) -> Option<Vec<u8>> {
    let revision = file.revision().ok()?;
    let reader = RangeReader::new(file.capability(), &revision, ARCHIVE_READ_BYTES);
    let mut archive = zip::ZipArchive::new(BufReader::with_capacity(16 * 1024, reader)).ok()?;
    let path = match kind {
        ArchiveKind::Mod => mod_metadata(&mut archive).1?,
        ArchiveKind::ResourcePack => "pack.png".to_string(),
    };
    entry_bytes(&mut archive, &path, ICON_MAX_BYTES).filter(|bytes| is_png(bytes))
}

pub(crate) fn is_png(bytes: &[u8]) -> bool {
    bytes.starts_with(b"\x89PNG\r\n\x1a\n")
}

type Archive<'a> = zip::ZipArchive<BufReader<RangeReader<'a>>>;

fn mod_metadata(archive: &mut Archive<'_>) -> (ContentFileMetadata, Option<String>) {
    if let Some(bytes) = entry_bytes(archive, "fabric.mod.json", METADATA_MAX_BYTES) {
        return fabric_metadata(&bytes);
    }
    if let Some(bytes) = entry_bytes(archive, "quilt.mod.json", METADATA_MAX_BYTES) {
        return quilt_metadata(&bytes);
    }
    for path in ["META-INF/neoforge.mods.toml", "META-INF/mods.toml"] {
        if let Some(bytes) = entry_bytes(archive, path, METADATA_MAX_BYTES) {
            let (mut metadata, icon) = forge_metadata(&String::from_utf8_lossy(&bytes));
            if metadata.version.is_none() {
                metadata.version = entry_bytes(archive, "META-INF/MANIFEST.MF", METADATA_MAX_BYTES)
                    .and_then(|manifest| {
                        implementation_version(&String::from_utf8_lossy(&manifest))
                    });
            }
            return (metadata, icon);
        }
    }
    (ContentFileMetadata::default(), None)
}

fn fabric_metadata(bytes: &[u8]) -> (ContentFileMetadata, Option<String>) {
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(bytes) else {
        return (ContentFileMetadata::default(), None);
    };
    let authors = value["authors"]
        .as_array()
        .map(|authors| {
            authors
                .iter()
                .filter_map(|author| author.as_str().or_else(|| author["name"].as_str()))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    (
        ContentFileMetadata {
            title: value["name"]
                .as_str()
                .and_then(|text| clean(text, TITLE_CHARS)),
            version: value["version"]
                .as_str()
                .and_then(|text| clean(text, VERSION_CHARS)),
            authors: clean_authors(authors),
            description: value["description"]
                .as_str()
                .and_then(|text| clean(text, DESCRIPTION_CHARS)),
            icon: false,
        },
        icon_path(&value["icon"]),
    )
}

fn quilt_metadata(bytes: &[u8]) -> (ContentFileMetadata, Option<String>) {
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(bytes) else {
        return (ContentFileMetadata::default(), None);
    };
    let loader = &value["quilt_loader"];
    let metadata = &loader["metadata"];
    let authors = metadata["contributors"]
        .as_object()
        .map(|contributors| contributors.keys().map(String::as_str).collect())
        .unwrap_or_default();
    (
        ContentFileMetadata {
            title: metadata["name"]
                .as_str()
                .and_then(|text| clean(text, TITLE_CHARS)),
            version: loader["version"]
                .as_str()
                .and_then(|text| clean(text, VERSION_CHARS)),
            authors: clean_authors(authors),
            description: metadata["description"]
                .as_str()
                .and_then(|text| clean(text, DESCRIPTION_CHARS)),
            icon: false,
        },
        icon_path(&metadata["icon"]),
    )
}

/// Reads single-line string keys from the first `[[mods]]` table. Values the
/// loader substitutes at build time (`${...}`) are not display text.
fn forge_metadata(text: &str) -> (ContentFileMetadata, Option<String>) {
    let mut section = Section::Root;
    let mut metadata = ContentFileMetadata::default();
    let mut icon = None;
    let mut root_icon = None;
    for line in text.lines().take(4096) {
        let line = line.trim();
        if line.starts_with('[') {
            section = match (section, line) {
                (Section::Root, "[[mods]]") => Section::Mod,
                _ => Section::Other,
            };
            continue;
        }
        let Some((key, value)) = toml_string(line) else {
            continue;
        };
        match (section, key) {
            (Section::Root, "logoFile") => root_icon = relative_png(&value),
            (Section::Root, "authors") if metadata.authors.is_empty() => {
                metadata.authors = clean_authors(value.split(',').collect());
            }
            (Section::Mod, "displayName") => metadata.title = clean(&value, TITLE_CHARS),
            (Section::Mod, "version") => metadata.version = clean(&value, VERSION_CHARS),
            (Section::Mod, "authors") => {
                metadata.authors = clean_authors(value.split(',').collect());
            }
            (Section::Mod, "description") => {
                metadata.description = clean(&value, DESCRIPTION_CHARS);
            }
            (Section::Mod, "logoFile") => icon = relative_png(&value),
            _ => {}
        }
    }
    (metadata, icon.or(root_icon))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Section {
    Root,
    Mod,
    Other,
}

fn toml_string(line: &str) -> Option<(&str, String)> {
    let (key, rest) = line.split_once('=')?;
    let key = key.trim();
    let rest = rest.trim_start();
    let quote = rest.chars().next().filter(|c| *c == '"' || *c == '\'')?;
    if rest.starts_with("'''") || rest.starts_with("\"\"\"") {
        return None;
    }
    let mut value = String::new();
    let mut chars = rest[1..].chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' if quote == '"' => value.push(chars.next()?),
            c if c == quote => return Some((key, value)),
            c => value.push(c),
        }
    }
    None
}

fn implementation_version(manifest: &str) -> Option<String> {
    manifest
        .lines()
        .find_map(|line| line.strip_prefix("Implementation-Version:"))
        .and_then(|value| clean(value, VERSION_CHARS))
}

fn pack_metadata(bytes: &[u8]) -> ContentFileMetadata {
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(bytes) else {
        return ContentFileMetadata::default();
    };
    let mut description = String::new();
    component_text(&value["pack"]["description"], 0, &mut description);
    ContentFileMetadata {
        description: clean(&description, DESCRIPTION_CHARS),
        ..ContentFileMetadata::default()
    }
}

/// Flattens a Minecraft text component without following unbounded nesting.
fn component_text(value: &serde_json::Value, depth: usize, out: &mut String) {
    if depth > 8 || out.len() > DESCRIPTION_CHARS * 4 {
        return;
    }
    match value {
        serde_json::Value::String(text) => out.push_str(text),
        serde_json::Value::Array(parts) => {
            for part in parts.iter().take(64) {
                component_text(part, depth + 1, out);
            }
        }
        serde_json::Value::Object(map) => {
            if let Some(text) = map.get("text") {
                component_text(text, depth + 1, out);
            }
            if let Some(extra) = map.get("extra") {
                component_text(extra, depth + 1, out);
            }
        }
        _ => {}
    }
}

pub(crate) fn pack_directory_metadata(mcmeta: Option<&[u8]>, icon: bool) -> ContentFileMetadata {
    ContentFileMetadata {
        icon,
        ..mcmeta.map(pack_metadata).unwrap_or_default()
    }
}

/// Fabric and Quilt accept a path or a size-keyed map; prefer the largest.
fn icon_path(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(path) => relative_png(path),
        serde_json::Value::Object(sizes) => sizes
            .iter()
            .filter_map(|(size, path)| Some((size.parse::<u32>().ok()?, path.as_str()?)))
            .max_by_key(|(size, _)| *size)
            .and_then(|(_, path)| relative_png(path)),
        _ => None,
    }
}

fn relative_png(path: &str) -> Option<String> {
    let path = path.trim().trim_start_matches("./");
    let safe = !path.is_empty()
        && path.len() <= 256
        && !path.starts_with('/')
        && !path.contains('\\')
        && path.split('/').all(|part| !part.is_empty() && part != "..")
        && path.to_ascii_lowercase().ends_with(".png");
    safe.then(|| path.to_string())
}

fn clean_authors(authors: Vec<&str>) -> Vec<String> {
    authors
        .into_iter()
        .filter_map(|author| clean(author, AUTHOR_CHARS))
        .take(AUTHORS)
        .collect()
}

/// Display text: formatting codes and control characters removed, whitespace
/// collapsed, length bounded. Build-time placeholders are not display text.
fn clean(text: &str, max_chars: usize) -> Option<String> {
    if text.contains("${") {
        return None;
    }
    let mut out = String::new();
    let mut chars = text.chars();
    let mut space = false;
    let mut count = 0;
    while let Some(c) = chars.next() {
        if c == '§' {
            chars.next();
            continue;
        }
        if c.is_whitespace() || c.is_control() {
            space = !out.is_empty();
            continue;
        }
        if count == max_chars {
            out.push('…');
            break;
        }
        if space {
            out.push(' ');
            count += 1;
            space = false;
        }
        out.push(c);
        count += 1;
    }
    (!out.is_empty()).then_some(out)
}

fn entry_exists(archive: &mut Archive<'_>, path: &str) -> bool {
    archive.index_for_name(path).is_some()
}

fn entry_bytes(archive: &mut Archive<'_>, path: &str, max: u64) -> Option<Vec<u8>> {
    let entry = archive.by_name(path).ok()?;
    if !entry.is_file() || entry.size() > max {
        return None;
    }
    let mut bytes = Vec::new();
    entry.take(max + 1).read_to_end(&mut bytes).ok()?;
    (bytes.len() as u64 <= max).then_some(bytes)
}

/// Seekable reads through revision-checked ranges, refused past its allowance.
struct RangeReader<'a> {
    file: &'a FileCapability,
    revision: &'a FileRevision,
    size: u64,
    position: u64,
    allowance: u64,
    consumed: u64,
}

impl<'a> RangeReader<'a> {
    fn new(file: &'a FileCapability, revision: &'a FileRevision, allowance: u64) -> Self {
        Self {
            file,
            revision,
            size: revision.size(),
            position: 0,
            allowance,
            consumed: 0,
        }
    }
}

impl Read for RangeReader<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let remaining = self.size.saturating_sub(self.position);
        let length = (buffer.len() as u64).min(remaining).min(RANGE_BYTES as u64);
        if length == 0 {
            return Ok(0);
        }
        if self.consumed + length > self.allowance {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "archive metadata exceeds its read bound",
            ));
        }
        let bytes = self
            .file
            .read_range_bounded(self.revision, self.position, length as usize)?;
        buffer[..bytes.len()].copy_from_slice(&bytes);
        self.position += bytes.len() as u64;
        self.consumed += bytes.len() as u64;
        Ok(bytes.len())
    }
}

impl Seek for RangeReader<'_> {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        let next = match position {
            SeekFrom::Start(offset) => i128::from(offset),
            SeekFrom::End(delta) => i128::from(self.size) + i128::from(delta),
            SeekFrom::Current(delta) => i128::from(self.position) + i128::from(delta),
        };
        if !(0..=i128::from(self.size)).contains(&next) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "archive seek escaped the file",
            ));
        }
        self.position = next as u64;
        Ok(self.position)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::io::Write;

    pub(crate) fn jar(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut bytes = io::Cursor::new(Vec::new());
        let mut zip = zip::ZipWriter::new(&mut bytes);
        for (name, content) in entries {
            zip.start_file(*name, zip::write::SimpleFileOptions::default())
                .unwrap();
            zip.write_all(content).unwrap();
        }
        zip.finish().unwrap();
        bytes.into_inner()
    }

    #[test]
    fn loader_metadata_yields_bounded_display_text_and_safe_icon_paths() {
        let (fabric, icon) = fabric_metadata(
            br#"{"name":"\u00a7aSodium","version":"0.6.13+mc1.21.4",
                "authors":["JellySquid",{"name":"IMS"}],"icon":{"64":"a/s.png","128":"a/l.png"}}"#,
        );
        assert_eq!(fabric.title.as_deref(), Some("Sodium"));
        assert_eq!(fabric.version.as_deref(), Some("0.6.13+mc1.21.4"));
        assert_eq!(fabric.authors, ["JellySquid", "IMS"]);
        assert_eq!(icon.as_deref(), Some("a/l.png"));

        let (quilt, icon) = quilt_metadata(
            br#"{"quilt_loader":{"version":"1.2.0","metadata":{"name":"Q",
                "contributors":{"Ada":"Owner"},"icon":"../escape.png"}}}"#,
        );
        assert_eq!(
            (quilt.title.as_deref(), quilt.version.as_deref()),
            (Some("Q"), Some("1.2.0"))
        );
        assert_eq!(quilt.authors, ["Ada"]);
        assert_eq!(icon, None);

        let (forge, icon) = forge_metadata(
            "modLoader=\"javafml\"\nlogoFile=\"root.png\"\n[[mods]]\nmodId=\"create\"\n\
             version=\"${file.jarVersion}\"\ndisplayName=\"Create\" # name\n\
             authors=\"simibubi, Kryppers\"\n[[dependencies.create]]\ndisplayName=\"Other\"\n",
        );
        assert_eq!(forge.title.as_deref(), Some("Create"));
        assert_eq!(forge.version, None);
        assert_eq!(forge.authors, ["simibubi", "Kryppers"]);
        assert_eq!(icon.as_deref(), Some("root.png"));

        let long = format!("{}\n\u{7}tail", "x".repeat(400));
        let cleaned = clean(&long, TITLE_CHARS).unwrap();
        assert_eq!(cleaned.chars().count(), TITLE_CHARS + 1);
        assert!(cleaned.ends_with('…'));
        assert_eq!(clean(" \u{a7}l  \t", TITLE_CHARS), None);
    }

    #[test]
    fn pack_descriptions_flatten_text_components() {
        let metadata = pack_metadata(
            br#"{"pack":{"pack_format":34,"description":[{"text":"Faithful ","extra":[{"text":"32x"}]}," \u00a77pack"]}}"#,
        );
        assert_eq!(metadata.description.as_deref(), Some("Faithful 32x pack"));
    }

    fn with_file(bytes: &[u8], test: impl FnOnce(&ScopedFile)) {
        use crate::library::{LibraryLifecycle, LibraryOpenOutcome};
        let directory = tempfile::tempdir().unwrap();
        let physical = directory.path().canonicalize().unwrap();
        std::fs::write(physical.join("file.jar"), bytes).unwrap();
        let owner = match LibraryLifecycle::open(&physical) {
            LibraryOpenOutcome::Ready(owner) => owner,
            other => panic!("fixture acquisition failed: {other:?}"),
        };
        let root = owner.admit().unwrap().files().unwrap();
        let file = root
            .open_file(&crate::files::PortableName::new_exact("file.jar").unwrap())
            .unwrap();
        test(&file);
        drop(file);
        drop(root);
        owner.close_admission();
        assert!(matches!(
            owner.revoke_application_root().unwrap(),
            axial_fs::RootRevokeOutcome::Revoked
        ));
    }

    #[test]
    fn archives_are_read_within_the_listing_budget() {
        let png = b"\x89PNG\r\n\x1a\nicon";
        let bytes = jar(&[
            (
                "fabric.mod.json",
                br#"{"name":"Mod Menu","version":"11.0.3","icon":"assets/modmenu/icon.png"}"#,
            ),
            ("assets/modmenu/icon.png", png),
        ]);
        with_file(&bytes, |file| {
            let mut budget = ReadBudget::listing();
            let metadata = archive_metadata(file, ArchiveKind::Mod, &mut budget);
            assert_eq!(metadata.title.as_deref(), Some("Mod Menu"));
            assert!(metadata.icon);
            assert!(budget.remaining < LISTING_READ_BYTES);
            assert_eq!(
                archive_icon(file, ArchiveKind::Mod).as_deref(),
                Some(&png[..])
            );

            let mut exhausted = ReadBudget::bytes(16);
            assert_eq!(
                archive_metadata(file, ArchiveKind::Mod, &mut exhausted),
                ContentFileMetadata::default()
            );
            assert_eq!(exhausted.remaining, 0);
        });
        with_file(b"not a zip", |file| {
            let mut budget = ReadBudget::listing();
            assert_eq!(
                archive_metadata(file, ArchiveKind::ResourcePack, &mut budget),
                ContentFileMetadata::default()
            );
            assert_eq!(archive_icon(file, ArchiveKind::ResourcePack), None);
        });
    }
}
