//! Modrinth modpack (`.mrpack`) import. A pack is a zip holding an index of
//! files to fetch plus an `overrides/` tree to copy in verbatim. It is not
//! content you add to an instance — it *is* an instance, so this materializes
//! one rather than dropping a file in a folder.
//!
//! Every path out of the archive is untrusted. A pack that names
//! `../../../.ssh/authorized_keys` must not be able to write there, so both the
//! indexed downloads and the overrides go through the same containment check.

use crate::error::{ContentError, ContentResult};
use crate::model::{ContentKind, FileRef};
use crate::transaction::{FileTransaction, StagingGuard};
use axial_minecraft::download::{
    DownloadProgress, ExpectedIntegrity, download_file_with_client_report,
};
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};

const INDEX_FILE: &str = "modrinth.index.json";
const OVERRIDES: &str = "overrides";
const CLIENT_OVERRIDES: &str = "client-overrides";
const SUPPORTED_FORMAT_VERSION: u32 = 1;
#[cfg(not(test))]
const MAX_INDEX_BYTES: u64 = 8 << 20;
#[cfg(test)]
const MAX_INDEX_BYTES: u64 = 1024;
#[cfg(not(test))]
const MAX_OVERRIDE_ENTRY_BYTES: u64 = 128 << 20;
#[cfg(test)]
const MAX_OVERRIDE_ENTRY_BYTES: u64 = 1024;
#[cfg(not(test))]
const MAX_OVERRIDE_TOTAL_BYTES: u64 = 512 << 20;
#[cfg(test)]
const MAX_OVERRIDE_TOTAL_BYTES: u64 = 2048;
const MAX_OVERRIDE_FILES: usize = 10_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackLoader {
    /// Launcher loader short key: `fabric`, `forge`, `neoforge` or `quilt`.
    pub key: String,
    pub version: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackFile {
    /// Game-dir-relative destination, already checked for containment.
    pub path: String,
    pub url: String,
    pub sha1: Option<String>,
    pub sha512: Option<String>,
    pub size: Option<u64>,
}

impl PackFile {
    pub fn kind(&self) -> Option<ContentKind> {
        match self.path.split('/').next()? {
            "mods" => Some(ContentKind::Mod),
            "resourcepacks" => Some(ContentKind::ResourcePack),
            "shaderpacks" => Some(ContentKind::ShaderPack),
            _ => None,
        }
    }

    pub fn filename(&self) -> &str {
        self.path.rsplit('/').next().unwrap_or(&self.path)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackIndex {
    pub name: String,
    pub version: String,
    pub minecraft: String,
    pub loader: Option<PackLoader>,
    pub files: Vec<PackFile>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackInstallReport {
    pub index: PackIndex,
    /// Files that landed on disk, in game-dir-relative form.
    pub installed: Vec<PackFile>,
    pub overrides_applied: usize,
}

/// Read a pack's index without installing anything, so a caller can learn the
/// loader and Minecraft version it needs before creating an instance for it.
pub fn read_pack_index(archive: &Path) -> ContentResult<PackIndex> {
    let file = fs::File::open(archive)?;
    let mut zip = zip::ZipArchive::new(file)
        .map_err(|error| ContentError::Invalid(format!("not a readable modpack: {error}")))?;
    let mut entry = zip
        .by_name(INDEX_FILE)
        .map_err(|_| ContentError::Invalid("modpack has no modrinth.index.json".to_string()))?;
    if entry.size() > MAX_INDEX_BYTES {
        return Err(ContentError::Invalid(
            "modpack index exceeds the size limit".to_string(),
        ));
    }
    let mut raw = String::new();
    (&mut entry)
        .take(MAX_INDEX_BYTES + 1)
        .read_to_string(&mut raw)?;
    if raw.len() as u64 > MAX_INDEX_BYTES {
        return Err(ContentError::Invalid(
            "modpack index exceeds the size limit".to_string(),
        ));
    }
    parse_pack_index(&raw)
}

/// Materialize a pack into `game_dir`: fetch every indexed file through the
/// verified downloader, then lay the overrides on top (client overrides last, so
/// they win).
pub async fn install_pack<F>(
    client: &reqwest::Client,
    game_dir: &Path,
    archive: &Path,
    on_progress: F,
) -> ContentResult<PackInstallReport>
where
    F: FnMut(DownloadProgress),
{
    install_pack_files(client, game_dir, archive, &[], true, on_progress).await
}

/// Install either the full pack or an explicit set of indexed paths. Overrides
/// are opt-in so cherry-picking files into an existing instance never silently
/// replaces its configuration.
pub async fn install_pack_files<F>(
    client: &reqwest::Client,
    game_dir: &Path,
    archive: &Path,
    selected_paths: &[String],
    include_overrides: bool,
    on_progress: F,
) -> ContentResult<PackInstallReport>
where
    F: FnMut(DownloadProgress),
{
    install_pack_files_with_finalize(
        client,
        game_dir,
        archive,
        selected_paths,
        include_overrides,
        on_progress,
        |_, _| Ok(()),
    )
    .await
}

pub async fn install_pack_files_with_finalize<F, P>(
    client: &reqwest::Client,
    game_dir: &Path,
    archive: &Path,
    selected_paths: &[String],
    include_overrides: bool,
    mut on_progress: F,
    finalize: P,
) -> ContentResult<PackInstallReport>
where
    F: FnMut(DownloadProgress),
    P: FnOnce(&PackInstallReport, &mut PackFinalizeContext<'_>) -> ContentResult<()>,
{
    let index = read_pack_index(archive)?;
    let selected: HashSet<&str> = selected_paths.iter().map(String::as_str).collect();
    if !selected.is_empty() && include_overrides {
        return Err(ContentError::Invalid(
            "modpack overrides cannot be applied with selected files".to_string(),
        ));
    }
    let files: Vec<&PackFile> = index
        .files
        .iter()
        .filter(|file| selected.is_empty() || selected.contains(file.path.as_str()))
        .collect();
    if !selected.is_empty() && files.len() != selected.len() {
        return Err(ContentError::Invalid(
            "the selected modpack files changed; review the pack again".to_string(),
        ));
    }
    if !selected.is_empty() {
        reject_occupied_pack_destinations(game_dir, files.iter().map(|file| file.path.as_str()))?;
    }
    let total = files.len() as i32;
    let mut installed = Vec::with_capacity(files.len());
    let staging = StagingGuard::create(game_dir, "axial-pack-stage")?;
    let mut relative_paths = Vec::with_capacity(files.len());

    for (position, file) in files.into_iter().enumerate() {
        let destination = contained_path(staging.path(), &file.path)?;
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)?;
        }

        on_progress(progress(
            "download",
            position as i32,
            total,
            Some(file.filename().to_string()),
        ));

        let expected = ExpectedIntegrity {
            size: file.size,
            sha1: file.sha1.clone(),
        };
        download_file_with_client_report(client, &file.url, &destination, &expected)
            .await
            .map_err(|error| ContentError::Download(error.into_download_error().to_string()))?;
        installed.push(file.clone());
        relative_paths.push(file.path.clone());
    }

    let overrides_applied = if include_overrides {
        on_progress(progress("overrides", total, total, None));
        let overrides = apply_overrides(staging.path(), archive)?;
        let indexed: HashSet<&str> = relative_paths.iter().map(String::as_str).collect();
        if overrides
            .iter()
            .any(|relative| indexed.contains(relative.as_str()))
        {
            return Err(ContentError::Invalid(
                "modpack override replaces an indexed content file".to_string(),
            ));
        }
        let count = overrides.len();
        relative_paths.extend(overrides);
        count
    } else {
        0
    };

    relative_paths.sort();
    relative_paths.dedup();
    on_progress(progress("commit", total, total, None));
    let mut transaction = if selected.is_empty() {
        FileTransaction::apply(game_dir, staging.transfer(), &relative_paths)?
    } else {
        reject_occupied_pack_destinations(game_dir, relative_paths.iter().map(String::as_str))?;
        FileTransaction::apply_new(game_dir, staging.transfer(), &relative_paths)?
    };
    let report = PackInstallReport {
        index,
        installed,
        overrides_applied,
    };
    let finalize_result = {
        let mut context = PackFinalizeContext {
            transaction: &mut transaction,
        };
        finalize(&report, &mut context)
    };
    if let Err(error) = finalize_result {
        transaction.rollback();
        return Err(error);
    }
    transaction.commit();

    on_progress(done(total));
    Ok(report)
}

fn reject_occupied_pack_destinations<'a>(
    game_dir: &Path,
    relative_paths: impl IntoIterator<Item = &'a str>,
) -> ContentResult<()> {
    for relative in relative_paths {
        let mut variants = vec![relative.to_string()];
        if matches!(
            relative.split('/').next(),
            Some("mods" | "resourcepacks" | "shaderpacks")
        ) && !relative.ends_with(".disabled")
        {
            variants.push(format!("{relative}.disabled"));
        }
        for variant in variants {
            let destination = contained_path(game_dir, &variant)?;
            match fs::symlink_metadata(destination) {
                Ok(_) => {
                    return Err(ContentError::Invalid(
                        "a selected modpack destination is already occupied".to_string(),
                    ));
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(ContentError::Io(error)),
            }
        }
    }
    Ok(())
}

/// Filesystem changes that must be committed with a pack import. Stale managed
/// files are moved into the pack transaction's backup and restored if the
/// manifest finalizer fails.
pub struct PackFinalizeContext<'a> {
    transaction: &'a mut FileTransaction,
}

impl PackFinalizeContext<'_> {
    pub fn stage_removals(&mut self, relative_paths: &[String]) -> ContentResult<()> {
        self.transaction.stage_removals(relative_paths)
    }
}

fn apply_overrides(game_dir: &Path, archive: &Path) -> ContentResult<Vec<String>> {
    let file = fs::File::open(archive)?;
    let mut zip = zip::ZipArchive::new(file)
        .map_err(|error| ContentError::Invalid(format!("not a readable modpack: {error}")))?;

    let mut applied = Vec::new();
    let mut extracted_bytes = 0_u64;
    // Client overrides go last: where both define a file, the client copy wins.
    for root in [OVERRIDES, CLIENT_OVERRIDES] {
        let prefix = format!("{root}/");
        for index in 0..zip.len() {
            let mut entry = zip
                .by_index(index)
                .map_err(|error| ContentError::Invalid(format!("unreadable modpack: {error}")))?;
            if entry.is_dir() {
                continue;
            }
            let Some(name) = entry.enclosed_name().map(|path| path.to_path_buf()) else {
                continue;
            };
            let Some(relative) = name
                .to_string_lossy()
                .strip_prefix(&prefix)
                .map(str::to_string)
            else {
                continue;
            };
            if relative.is_empty() {
                continue;
            }
            if applied.len() >= MAX_OVERRIDE_FILES {
                return Err(ContentError::Invalid(
                    "modpack contains too many override files".to_string(),
                ));
            }
            let declared_size = entry.size();
            if declared_size > MAX_OVERRIDE_ENTRY_BYTES
                || extracted_bytes.saturating_add(declared_size) > MAX_OVERRIDE_TOTAL_BYTES
            {
                return Err(ContentError::Invalid(
                    "modpack overrides exceed the extraction limit".to_string(),
                ));
            }

            let destination = contained_path(game_dir, &relative)?;
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent)?;
            }
            let mut sink = fs::File::create(&destination)?;
            let remaining_total = MAX_OVERRIDE_TOTAL_BYTES.saturating_sub(extracted_bytes);
            let copy_limit = MAX_OVERRIDE_ENTRY_BYTES.min(remaining_total);
            let copied = io::copy(&mut (&mut entry).take(copy_limit + 1), &mut sink)?;
            if copied > copy_limit {
                return Err(ContentError::Invalid(
                    "modpack overrides exceed the extraction limit".to_string(),
                ));
            }
            extracted_bytes = extracted_bytes.saturating_add(copied);
            applied.push(relative);
        }
    }
    Ok(applied)
}

/// Resolve `relative` under `root`, refusing anything that would escape it.
fn contained_path(root: &Path, relative: &str) -> ContentResult<PathBuf> {
    let candidate = Path::new(relative);
    if candidate.is_absolute() {
        return Err(ContentError::Invalid(format!(
            "modpack file escapes the instance: {relative}"
        )));
    }
    let mut resolved = root.to_path_buf();
    for component in candidate.components() {
        match component {
            Component::Normal(part) => resolved.push(part),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(ContentError::Invalid(format!(
                    "modpack file escapes the instance: {relative}"
                )));
            }
        }
    }
    if !resolved.starts_with(root) {
        return Err(ContentError::Invalid(format!(
            "modpack file escapes the instance: {relative}"
        )));
    }
    Ok(resolved)
}

pub fn parse_pack_index(raw: &str) -> ContentResult<PackIndex> {
    let dto: dto::Index = serde_json::from_str(raw)?;
    if dto.format_version > SUPPORTED_FORMAT_VERSION {
        return Err(ContentError::Invalid(format!(
            "this modpack needs a newer launcher (format {})",
            dto.format_version
        )));
    }

    let minecraft = dto
        .dependencies
        .get("minecraft")
        .cloned()
        .unwrap_or_default();
    if minecraft.is_empty() {
        return Err(ContentError::Invalid(
            "modpack does not say which Minecraft version it needs".to_string(),
        ));
    }

    let loader = loader_from_dependencies(&dto.dependencies);
    let files = dto
        .files
        .into_iter()
        .filter(|file| file.included_on_client())
        .map(pack_file)
        .collect::<ContentResult<Vec<PackFile>>>()?;

    Ok(PackIndex {
        name: dto.name,
        version: dto.version_id,
        minecraft,
        loader,
        files,
    })
}

fn pack_file(file: dto::IndexFile) -> ContentResult<PackFile> {
    let url = file
        .downloads
        .into_iter()
        .find(|url| url.starts_with("https://"))
        .ok_or_else(|| {
            ContentError::Invalid(format!("modpack file has no download: {}", file.path))
        })?;
    Ok(PackFile {
        path: file.path,
        url,
        sha1: file.hashes.sha1,
        sha512: file.hashes.sha512,
        size: file.file_size,
    })
}

fn loader_from_dependencies(dependencies: &HashMap<String, String>) -> Option<PackLoader> {
    [
        ("fabric-loader", "fabric"),
        ("quilt-loader", "quilt"),
        ("neoforge", "neoforge"),
        ("forge", "forge"),
    ]
    .into_iter()
    .find_map(|(key, short)| {
        dependencies
            .get(key)
            .filter(|version| !version.is_empty())
            .map(|version| PackLoader {
                key: short.to_string(),
                version: version.clone(),
            })
    })
}

/// The pack's own archive, as a file to download and verify.
pub fn pack_archive_file(file: &FileRef) -> ExpectedIntegrity {
    ExpectedIntegrity {
        size: file.size,
        sha1: file.sha1.clone(),
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

mod dto {
    use super::*;

    #[derive(Debug, Deserialize)]
    #[serde(rename_all = "camelCase")]
    pub struct Index {
        #[serde(default)]
        pub format_version: u32,
        #[serde(default)]
        pub name: String,
        #[serde(default)]
        pub version_id: String,
        #[serde(default)]
        pub dependencies: HashMap<String, String>,
        #[serde(default)]
        pub files: Vec<IndexFile>,
    }

    #[derive(Debug, Deserialize)]
    #[serde(rename_all = "camelCase")]
    pub struct IndexFile {
        pub path: String,
        #[serde(default)]
        pub hashes: Hashes,
        #[serde(default)]
        pub env: Option<Env>,
        #[serde(default)]
        pub downloads: Vec<String>,
        #[serde(default)]
        pub file_size: Option<u64>,
    }

    impl IndexFile {
        /// Server-only files are dead weight in a client instance.
        pub fn included_on_client(&self) -> bool {
            self.env
                .as_ref()
                .and_then(|env| env.client.as_deref())
                .map(|client| client != "unsupported")
                .unwrap_or(true)
        }
    }

    #[derive(Debug, Default, Deserialize)]
    pub struct Hashes {
        #[serde(default)]
        pub sha1: Option<String>,
        #[serde(default)]
        pub sha512: Option<String>,
    }

    #[derive(Debug, Deserialize)]
    pub struct Env {
        #[serde(default)]
        pub client: Option<String>,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    const INDEX: &str = r#"{
        "formatVersion": 1,
        "game": "minecraft",
        "versionId": "2.1.0",
        "name": "Test Pack",
        "dependencies": { "minecraft": "1.21.6", "fabric-loader": "0.17.2" },
        "files": [
            {
                "path": "mods/sodium.jar",
                "hashes": { "sha1": "aaa", "sha512": "bbb" },
                "env": { "client": "required", "server": "unsupported" },
                "downloads": ["https://cdn.modrinth.com/sodium.jar"],
                "fileSize": 1024
            },
            {
                "path": "mods/server-only.jar",
                "hashes": {},
                "env": { "client": "unsupported", "server": "required" },
                "downloads": ["https://cdn.modrinth.com/server.jar"]
            },
            {
                "path": "shaderpacks/complementary.zip",
                "hashes": {},
                "downloads": ["https://cdn.modrinth.com/shader.zip"]
            }
        ]
    }"#;

    #[test]
    fn parses_loader_version_and_client_files() {
        let index = parse_pack_index(INDEX).expect("parse");

        assert_eq!(index.name, "Test Pack");
        assert_eq!(index.version, "2.1.0");
        assert_eq!(index.minecraft, "1.21.6");
        assert_eq!(
            index.loader,
            Some(PackLoader {
                key: "fabric".to_string(),
                version: "0.17.2".to_string()
            })
        );

        let paths: Vec<&str> = index.files.iter().map(|file| file.path.as_str()).collect();
        assert_eq!(paths, ["mods/sodium.jar", "shaderpacks/complementary.zip"]);
    }

    #[test]
    fn maps_each_file_to_the_kind_its_directory_implies() {
        let index = parse_pack_index(INDEX).expect("parse");

        assert_eq!(index.files[0].kind(), Some(ContentKind::Mod));
        assert_eq!(index.files[0].filename(), "sodium.jar");
        assert_eq!(index.files[1].kind(), Some(ContentKind::ShaderPack));
    }

    #[test]
    fn a_pack_without_a_minecraft_version_is_rejected() {
        let raw = r#"{ "formatVersion": 1, "dependencies": {}, "files": [] }"#;
        assert!(parse_pack_index(raw).is_err());
    }

    #[test]
    fn a_future_format_version_is_rejected_rather_than_guessed_at() {
        let raw =
            r#"{ "formatVersion": 2, "dependencies": { "minecraft": "1.21.6" }, "files": [] }"#;
        assert!(parse_pack_index(raw).is_err());
    }

    #[test]
    fn a_file_with_no_https_download_is_rejected() {
        let raw = r#"{
            "formatVersion": 1,
            "dependencies": { "minecraft": "1.21.6" },
            "files": [{ "path": "mods/x.jar", "downloads": ["http://insecure/x.jar"] }]
        }"#;
        assert!(parse_pack_index(raw).is_err());
    }

    #[test]
    fn a_vanilla_pack_declares_no_loader() {
        let raw = r#"{
            "formatVersion": 1,
            "dependencies": { "minecraft": "1.21.6" },
            "files": []
        }"#;
        assert_eq!(parse_pack_index(raw).expect("parse").loader, None);
    }

    #[test]
    fn compressed_pack_index_is_bounded_before_parsing() {
        let archive = override_archive(
            "index-limit",
            &[(INDEX_FILE, vec![b' '; MAX_INDEX_BYTES as usize + 1])],
        );

        let error = read_pack_index(&archive).expect_err("oversized index must be rejected");
        assert!(error.to_string().contains("size limit"));

        let _ = fs::remove_file(archive);
    }

    #[test]
    fn paths_that_escape_the_instance_are_refused() {
        let root = Path::new("/instances/aurora");

        for escape in [
            "../../../etc/passwd",
            "mods/../../outside.jar",
            "/etc/passwd",
        ] {
            assert!(
                contained_path(root, escape).is_err(),
                "{escape} must not resolve"
            );
        }

        assert_eq!(
            contained_path(root, "mods/sodium.jar").expect("contained"),
            root.join("mods").join("sodium.jar")
        );
        assert_eq!(
            contained_path(root, "./config/sodium.json").expect("contained"),
            root.join("config").join("sodium.json")
        );
    }

    fn override_archive(name: &str, entries: &[(&str, Vec<u8>)]) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "axial-pack-overrides-{name}-{}-{}.mrpack",
            std::process::id(),
            crate::transaction::staging_dir(Path::new(""), "test")
                .file_name()
                .expect("sequence")
                .to_string_lossy()
        ));
        let file = fs::File::create(&path).expect("archive");
        let mut writer = zip::ZipWriter::new(file);
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        for (entry_name, bytes) in entries {
            writer.start_file(*entry_name, options).expect("entry");
            writer.write_all(bytes).expect("entry bytes");
        }
        writer.finish().expect("finish archive");
        path
    }

    #[test]
    fn override_entry_size_is_bounded() {
        let root = std::env::temp_dir().join("axial-pack-override-entry-limit");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("root");
        let archive = override_archive(
            "entry-limit",
            &[(
                "overrides/config/oversized.bin",
                vec![b'x'; MAX_OVERRIDE_ENTRY_BYTES as usize + 1],
            )],
        );

        assert!(apply_overrides(&root, &archive).is_err());

        let _ = fs::remove_file(archive);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn cumulative_override_size_is_bounded() {
        let root = std::env::temp_dir().join("axial-pack-override-total-limit");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("root");
        let archive = override_archive(
            "total-limit",
            &[
                (
                    "overrides/config/first.bin",
                    vec![b'a'; MAX_OVERRIDE_ENTRY_BYTES as usize],
                ),
                (
                    "overrides/config/second.bin",
                    vec![b'b'; MAX_OVERRIDE_ENTRY_BYTES as usize],
                ),
                ("overrides/config/third.bin", vec![b'c']),
            ],
        );

        assert!(apply_overrides(&root, &archive).is_err());

        let _ = fs::remove_file(archive);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn selected_pack_destinations_preserve_enabled_and_disabled_files() {
        let root = std::env::temp_dir().join("axial-pack-selected-occupied");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("mods")).expect("mods");

        fs::write(root.join("mods/example.jar"), b"enabled").expect("enabled");
        assert!(
            reject_occupied_pack_destinations(&root, ["mods/example.jar"].into_iter()).is_err()
        );
        fs::remove_file(root.join("mods/example.jar")).expect("remove enabled");
        fs::write(root.join("mods/example.jar.disabled"), b"disabled").expect("disabled");
        assert!(
            reject_occupied_pack_destinations(&root, ["mods/example.jar"].into_iter()).is_err()
        );

        let _ = fs::remove_dir_all(root);
    }
}
