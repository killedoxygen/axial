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

type Tree = serde_json::Value;

/// A file in a jar that describes the mod inside: where it is, how it is written, and where its
/// facts sit once read.
struct Manifest {
    path: &'static str,
    parse: fn(&[u8]) -> Option<Tree>,
    facts: for<'a> fn(&'a Tree) -> Facts<'a>,
}

/// Every loader's manifest, in the order they speak for a jar that carries several. The jar's own
/// comes last: it knows only the version a build left as a placeholder elsewhere.
const MANIFESTS: [Manifest; 6] = [
    Manifest {
        path: "fabric.mod.json",
        parse: lenient_json,
        facts: fabric,
    },
    Manifest {
        path: "quilt.mod.json",
        parse: lenient_json,
        facts: quilt,
    },
    Manifest {
        path: "META-INF/neoforge.mods.toml",
        parse: toml,
        facts: forge,
    },
    Manifest {
        path: "META-INF/mods.toml",
        parse: toml,
        facts: forge,
    },
    Manifest {
        path: "mcmod.info",
        parse: lenient_json,
        facts: legacy_forge,
    },
    Manifest {
        path: "META-INF/MANIFEST.MF",
        parse: jar_manifest,
        facts: build,
    },
];

/// What one manifest says, as written. `icons` are the places it names art, best first.
#[derive(Default)]
struct Facts<'a> {
    id: Option<&'a str>,
    title: Option<&'a str>,
    version: Option<&'a str>,
    authors: Vec<&'a str>,
    description: Option<&'a str>,
    icons: Vec<&'a str>,
}

/// Describe a mod from its manifests and name its icon. Each fact is the first manifest's that
/// states it, and the icon is the first named place that holds a file, so a manifest that is
/// unreadable, incomplete or wrong about its art costs only what it alone knew.
fn mod_metadata<R: Read + Seek>(
    archive: &mut zip::ZipArchive<R>,
) -> (ContentFileMetadata, Option<String>) {
    let mut metadata = ContentFileMetadata::default();
    let mut icon = None;
    let mut ids = Vec::new();
    for manifest in &MANIFESTS {
        let settled = metadata.title.is_some()
            && metadata.version.is_some()
            && metadata.description.is_some()
            && !metadata.authors.is_empty()
            && icon.is_some();
        if settled {
            break;
        }
        let Some(tree) = entry_bytes(archive, manifest.path, METADATA_MAX_BYTES)
            .and_then(|bytes| (manifest.parse)(&bytes))
        else {
            continue;
        };
        let facts = (manifest.facts)(&tree);
        state(&mut metadata.title, facts.title, TITLE_CHARS);
        state(&mut metadata.version, facts.version, VERSION_CHARS);
        state(
            &mut metadata.description,
            facts.description,
            DESCRIPTION_CHARS,
        );
        if metadata.authors.is_empty() {
            metadata.authors = clean_authors(facts.authors);
        }
        icon = icon.or_else(|| held(archive, facts.icons.into_iter().map(str::to_owned)));
        ids.extend(facts.id.map(str::to_owned));
    }
    // Mods that name no art usually keep it where the other loaders' builds do.
    let usual = ids
        .iter()
        .map(|id| format!("assets/{id}/icon.png"))
        .chain(["icon.png".to_owned()]);
    let icon = icon.or_else(|| held(archive, usual));
    (metadata, icon)
}

fn state(fact: &mut Option<String>, text: Option<&str>, max_chars: usize) {
    if fact.is_none() {
        *fact = text.and_then(|text| clean(text, max_chars));
    }
}

/// The first safe PNG path the archive holds a file at.
fn held<R: Read + Seek>(
    archive: &mut zip::ZipArchive<R>,
    places: impl IntoIterator<Item = String>,
) -> Option<String> {
    places
        .into_iter()
        .filter_map(|place| relative_png(&place))
        .find(|path| entry_exists(archive, path))
}

fn fabric(tree: &Tree) -> Facts<'_> {
    Facts {
        id: tree["id"].as_str(),
        title: tree["name"].as_str(),
        version: tree["version"].as_str(),
        authors: people(&tree["authors"]),
        description: tree["description"].as_str(),
        icons: sized(&tree["icon"]),
    }
}

fn quilt(tree: &Tree) -> Facts<'_> {
    let loader = &tree["quilt_loader"];
    let about = &loader["metadata"];
    Facts {
        id: loader["id"].as_str(),
        title: about["name"].as_str(),
        version: loader["version"].as_str(),
        authors: people(&about["contributors"]),
        description: about["description"].as_str(),
        icons: sized(&about["icon"]),
    }
}

/// Forge and NeoForge describe the first of the file's mods. What that mod leaves out may be
/// stated for the whole file, and its square icon is preferred to its wider logo.
fn forge(tree: &Tree) -> Facts<'_> {
    let about = &tree["mods"][0];
    let id = about["modId"].as_str();
    let stated = |key: &str| about[key].as_str().or_else(|| tree[key].as_str());
    Facts {
        id,
        title: about["displayName"].as_str(),
        version: about["version"].as_str(),
        authors: [&about["authors"], &tree["authors"]]
            .into_iter()
            .map(people)
            .find(|names| !names.is_empty())
            .unwrap_or_default(),
        description: about["description"].as_str(),
        icons: [
            stated("iconFile"),
            id.and_then(|id| tree["modproperties"][id]["catalogueImageIcon"].as_str()),
            stated("logoFile"),
        ]
        .into_iter()
        .flatten()
        .collect(),
    }
}

/// Forge before 1.13: a list of mods, sometimes wrapped with the list's own version.
fn legacy_forge(tree: &Tree) -> Facts<'_> {
    let about = &tree.get("modList").unwrap_or(tree)[0];
    Facts {
        id: about["modid"].as_str(),
        title: about["name"].as_str(),
        version: about["version"].as_str(),
        authors: people(&about["authorList"]),
        description: about["description"].as_str(),
        icons: about["logoFile"].as_str().into_iter().collect(),
    }
}

/// What the build recorded about the jar itself.
fn build(tree: &Tree) -> Facts<'_> {
    Facts {
        version: tree["Implementation-Version"].as_str(),
        ..Facts::default()
    }
}

/// Names however a manifest lists them: one string with commas, a list of names or of people, or
/// a table keyed by name.
fn people(value: &Tree) -> Vec<&str> {
    match value {
        Tree::String(names) => names.split(',').collect(),
        Tree::Array(list) => list
            .iter()
            .filter_map(|one| one.as_str().or_else(|| one["name"].as_str()))
            .collect(),
        Tree::Object(roles) => roles.keys().map(String::as_str).collect(),
        _ => Vec::new(),
    }
}

/// An icon given as one path, or as paths keyed by size: the largest first.
fn sized(value: &Tree) -> Vec<&str> {
    match value {
        Tree::String(path) => vec![path],
        Tree::Object(sizes) => {
            let mut sizes: Vec<(u32, &str)> = sizes
                .iter()
                .filter_map(|(size, path)| Some((size.parse().ok()?, path.as_str()?)))
                .collect();
            sizes.sort_by_key(|(size, _)| std::cmp::Reverse(*size));
            sizes.into_iter().map(|(_, path)| path).collect()
        }
        _ => Vec::new(),
    }
}

/// The game and its loaders read these files leniently, and mods ship line breaks typed straight
/// into a description. Strict JSON refuses a raw control character in a string, so a file that
/// fails is read once more with those escaped.
pub(crate) fn lenient_json(bytes: &[u8]) -> Option<Tree> {
    if let Ok(value) = serde_json::from_slice(bytes) {
        return Some(value);
    }
    let mut escaped = Vec::with_capacity(bytes.len() + 64);
    let (mut quoted, mut slash) = (false, false);
    for &byte in bytes {
        if quoted && byte < 0x20 {
            escaped.extend_from_slice(format!("\\u{byte:04x}").as_bytes());
            slash = false;
            continue;
        }
        if !quoted {
            quoted = byte == b'"';
        } else if slash {
            slash = false;
        } else if byte == b'\\' {
            slash = true;
        } else if byte == b'"' {
            quoted = false;
        }
        escaped.push(byte);
    }
    serde_json::from_slice(&escaped).ok()
}

fn toml(bytes: &[u8]) -> Option<Tree> {
    toml::from_str(&String::from_utf8_lossy(bytes)).ok()
}

/// A jar's own manifest: `Name: value` lines, of which the first block describes the jar.
fn jar_manifest(bytes: &[u8]) -> Option<Tree> {
    let text = String::from_utf8_lossy(bytes);
    Some(
        text.lines()
            .take_while(|line| !line.trim().is_empty())
            .filter_map(|line| line.split_once(':'))
            .map(|(name, value)| (name.to_owned(), Tree::from(value.trim())))
            .collect(),
    )
}

fn pack_metadata(bytes: &[u8]) -> ContentFileMetadata {
    let Some(value) = lenient_json(bytes) else {
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

fn entry_exists<R: Read + Seek>(archive: &mut zip::ZipArchive<R>, path: &str) -> bool {
    archive.index_for_name(path).is_some()
}

fn entry_bytes<R: Read + Seek>(
    archive: &mut zip::ZipArchive<R>,
    path: &str,
    max: u64,
) -> Option<Vec<u8>> {
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

    const PNG: &[u8] = b"\x89PNG\r\n\x1a\n";

    /// A mod jar's description and the icon it resolves to.
    fn described(entries: &[(&str, &[u8])]) -> (ContentFileMetadata, Option<String>) {
        mod_metadata(&mut zip::ZipArchive::new(io::Cursor::new(jar(entries))).unwrap())
    }

    #[test]
    fn every_loader_manifest_yields_bounded_display_text_and_a_held_icon() {
        // The largest icon a manifest names is not in the jar; the next is.
        let (fabric, icon) = described(&[
            (
                "fabric.mod.json",
                br#"{"id":"sodium","name":"\u00a7aSodium","version":"0.6.13+mc1.21.4",
                    "authors":["JellySquid",{"name":"IMS"}],"icon":{"64":"a/s.png","128":"a/l.png"}}"#,
            ),
            ("a/s.png", PNG),
        ]);
        assert_eq!(fabric.title.as_deref(), Some("Sodium"));
        assert_eq!(fabric.version.as_deref(), Some("0.6.13+mc1.21.4"));
        assert_eq!(fabric.authors, ["JellySquid", "IMS"]);
        assert_eq!(icon.as_deref(), Some("a/s.png"));

        // A line break typed into a description does not cost the rest of the file.
        let (loose, icon) = described(&[
            (
                "fabric.mod.json",
                b"{\"name\":\"EMF\",\"description\":\"One \\\"line\\\"\nand\tanother\",\"icon\":\"icon.png\"}",
            ),
            ("icon.png", PNG),
        ]);
        assert_eq!(loose.title.as_deref(), Some("EMF"));
        assert_eq!(
            loose.description.as_deref(),
            Some("One \"line\" and another")
        );
        assert_eq!(icon.as_deref(), Some("icon.png"));

        // An unsafe icon path is passed over for the place such art is usually kept.
        let (quilt, icon) = described(&[
            (
                "quilt.mod.json",
                br#"{"quilt_loader":{"id":"q","version":"1.2.0","metadata":{"name":"Q",
                    "contributors":{"Ada":"Owner"},"icon":"../escape.png"}}}"#,
            ),
            ("assets/q/icon.png", PNG),
        ]);
        assert_eq!(
            (quilt.title.as_deref(), quilt.version.as_deref()),
            (Some("Q"), Some("1.2.0"))
        );
        assert_eq!(quilt.authors, ["Ada"]);
        assert_eq!(icon.as_deref(), Some("assets/q/icon.png"));

        // Forge's template comments its tables, descriptions span lines, and a version the
        // build was to fill in is the jar's own.
        let (forge, icon) = described(&[
            (
                "META-INF/mods.toml",
                b"modLoader=\"javafml\" #mandatory\nlogoFile=\"root.png\"\n\
                  authors=\"simibubi, Kryppers\"\n[[mods]] #mandatory\nmodId=\"create\"\n\
                  version=\"${file.jarVersion}\"\ndisplayName=\"Create\" # name\n\
                  description='''\nBuild\n[things]\n'''\n[[dependencies.create]]\nmodId=\"forge\"\n",
            ),
            (
                "META-INF/MANIFEST.MF",
                b"Manifest-Version: 1.0\r\nImplementation-Version: 6.0.4\r\n\r\n\
                  Name: other\r\nImplementation-Version: 9\r\n",
            ),
            ("root.png", PNG),
        ]);
        assert_eq!(forge.title.as_deref(), Some("Create"));
        assert_eq!(forge.version.as_deref(), Some("6.0.4"));
        assert_eq!(forge.authors, ["simibubi", "Kryppers"]);
        assert_eq!(forge.description.as_deref(), Some("Build [things]"));
        assert_eq!(icon.as_deref(), Some("root.png"));

        // A broken manifest and a missing icon each cost only themselves.
        let (neoforge, icon) = described(&[
            ("fabric.mod.json", b"{\"name\":\"Cut"),
            (
                "META-INF/neoforge.mods.toml",
                b"[[mods]]\nmodId=\"iris\"\ndisplayName=\"Iris\"\nversion=\"1.0\"\n\
                  iconFile=\"gone.png\"\nlogoFile=\"logo.png\"\n",
            ),
            ("logo.png", PNG),
        ]);
        assert_eq!(neoforge.title.as_deref(), Some("Iris"));
        assert_eq!(icon.as_deref(), Some("logo.png"));

        let (legacy, icon) = described(&[(
            "mcmod.info",
            br#"[{"modid":"jei","name":"Just Enough Items","version":"4.24",
                "authorList":["mezz"],"logoFile":""}]"#,
        )]);
        assert_eq!(legacy.title.as_deref(), Some("Just Enough Items"));
        assert_eq!(legacy.authors, ["mezz"]);
        assert_eq!(icon, None);

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
