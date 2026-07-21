//! Modrinth modpack (`.mrpack`) import. A pack is a zip holding an index of
//! files to fetch plus an `overrides/` tree to copy in verbatim. It is not
//! content you add to an instance — it *is* an instance, so this materializes
//! one rather than dropping a file in a folder.
//!
//! Every path out of the archive is untrusted. A pack that names
//! `../../../.ssh/authorized_keys` must not be able to write there, so both the
//! indexed downloads and the overrides go through the same containment check.

use crate::error::{ContentError, ContentResult};
use crate::install::{ManagedRemoval, stage_managed_removals};
use crate::manifest::MANIFEST_FILE;
use crate::model::{ContentKind, FileRef};
use crate::transaction::{FileTransaction, StagingGuard};
use axial_minecraft::LoaderComponentId;
use axial_minecraft::download::{
    DownloadProgress, ExecutionDownloadFact, VerifiedContentIntegrity,
    download_verified_content_to_staging,
};
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::{Component, Path, PathBuf};
use std::time::Duration;
use url::{Host, Url};

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
const MAX_PACK_REDIRECTS: usize = 10;
const MAX_PACK_COORDINATE_BYTES: usize = 512;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct PackDownloadOrigin {
    host: String,
    port: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackLoader {
    pub component_id: LoaderComponentId,
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

#[derive(Debug, Clone, Copy)]
pub struct PackInstallOptions<'a> {
    pub selected_paths: &'a [String],
    pub include_overrides: bool,
}

/// Read a pack's index without installing anything, so a caller can learn the
/// loader and Minecraft version it needs before creating an instance for it.
pub fn read_pack_index(archive: &Path) -> ContentResult<PackIndex> {
    let file = fs::File::open(archive)?;
    let mut zip = zip::ZipArchive::new(file).map_err(|error| {
        ContentError::ProviderMetadataInvalid(format!("not a readable modpack: {error}"))
    })?;
    let mut entry = zip.by_name(INDEX_FILE).map_err(|_| {
        ContentError::ProviderMetadataInvalid("modpack has no modrinth.index.json".to_string())
    })?;
    if entry.size() > MAX_INDEX_BYTES {
        return Err(ContentError::ProviderMetadataInvalid(
            "modpack index exceeds the size limit".to_string(),
        ));
    }
    let mut raw = String::new();
    (&mut entry)
        .take(MAX_INDEX_BYTES + 1)
        .read_to_string(&mut raw)
        .map_err(|_| {
            ContentError::ProviderMetadataInvalid("modpack index could not be read".to_string())
        })?;
    if raw.len() as u64 > MAX_INDEX_BYTES {
        return Err(ContentError::ProviderMetadataInvalid(
            "modpack index exceeds the size limit".to_string(),
        ));
    }
    parse_pack_index(&raw)
}

/// Install either the full pack or an explicit set of indexed paths. Overrides
/// are opt-in so cherry-picking files into an existing instance never silently
/// replaces its configuration.
pub async fn install_pack_files_with_finalize<F, G, P>(
    game_dir: &Path,
    archive: &Path,
    options: PackInstallOptions<'_>,
    mut on_progress: F,
    mut on_download_fact: G,
    finalize: P,
) -> ContentResult<PackInstallReport>
where
    F: FnMut(DownloadProgress),
    G: FnMut(ExecutionDownloadFact),
    P: FnOnce(&PackInstallReport, &mut PackFinalizeContext<'_>) -> ContentResult<()>,
{
    let index = read_pack_index(archive)?;
    let selected: HashSet<&str> = options.selected_paths.iter().map(String::as_str).collect();
    if !selected.is_empty() && options.include_overrides {
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
        return Err(ContentError::ProviderMetadataInvalid(
            "the selected modpack files changed; review the pack again".to_string(),
        ));
    }
    reject_occupied_pack_destinations(game_dir, files.iter().map(|file| file.path.as_str()))?;
    let total = files.len() as i32;
    let mut installed = Vec::with_capacity(files.len());
    let staging = StagingGuard::create(game_dir, "axial-pack-stage")?;
    let mut relative_paths = Vec::with_capacity(files.len());
    let mut download_clients: HashMap<PackDownloadOrigin, reqwest::Client> = HashMap::new();

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

        let expected = VerifiedContentIntegrity {
            size: file.size,
            sha1: file.sha1.clone(),
            sha512: file.sha512.clone(),
        };
        let (_, origin) = validate_pack_download_url(&file.url)?;
        if !download_clients.contains_key(&origin) {
            let safe_client = build_pack_download_client(&file.url).await?;
            download_clients.insert(origin.clone(), safe_client);
        }
        let safe_client = download_clients
            .get(&origin)
            .expect("pack download client was inserted");
        match download_verified_content_to_staging(safe_client, &file.url, &destination, &expected)
            .await
        {
            Ok(report) => {
                installed.push(authenticated_pack_file(file, report.bytes_written));
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
        relative_paths.push(file.path.clone());
    }

    let overrides_applied = if options.include_overrides {
        on_progress(progress("overrides", total, total, None));
        let overrides = apply_overrides(staging.path(), archive)?;
        let indexed: HashSet<&str> = relative_paths.iter().map(String::as_str).collect();
        if overrides
            .iter()
            .any(|relative| indexed.contains(relative.as_str()))
        {
            return Err(ContentError::ProviderMetadataInvalid(
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
    reject_occupied_pack_destinations(game_dir, relative_paths.iter().map(String::as_str))?;
    let mut transaction =
        FileTransaction::apply_new(game_dir, staging.transfer(), &relative_paths)?;
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
        return match transaction.rollback() {
            Ok(()) => Err(error),
            Err(rollback_error) => Err(rollback_error),
        };
    }
    transaction.commit();

    on_progress(done(total));
    Ok(report)
}

fn authenticated_pack_file(file: &PackFile, bytes_written: u64) -> PackFile {
    let mut authenticated = file.clone();
    authenticated.size = Some(bytes_written);
    authenticated
}

fn validate_pack_download_url(raw: &str) -> ContentResult<(Url, PackDownloadOrigin)> {
    let url = Url::parse(raw).map_err(|_| {
        ContentError::ProviderMetadataInvalid("modpack download URL is invalid".to_string())
    })?;
    if url.scheme() != "https" || !url.username().is_empty() || url.password().is_some() {
        return Err(ContentError::ProviderMetadataInvalid(
            "modpack downloads require a public HTTPS URL".to_string(),
        ));
    }
    let host = url.host().ok_or_else(|| {
        ContentError::ProviderMetadataInvalid("modpack download URL has no host".to_string())
    })?;
    match host {
        Host::Ipv4(address) if !is_public_ip(IpAddr::V4(address)) => {
            return Err(ContentError::ProviderMetadataInvalid(
                "modpack download destination is not public".to_string(),
            ));
        }
        Host::Ipv6(address) if !is_public_ip(IpAddr::V6(address)) => {
            return Err(ContentError::ProviderMetadataInvalid(
                "modpack download destination is not public".to_string(),
            ));
        }
        Host::Domain(_) | Host::Ipv4(_) | Host::Ipv6(_) => {}
    }
    let port = url.port_or_known_default().ok_or_else(|| {
        ContentError::ProviderMetadataInvalid("modpack download URL has no usable port".to_string())
    })?;
    let host = host.to_string().to_ascii_lowercase();
    Ok((url, PackDownloadOrigin { host, port }))
}

async fn build_pack_download_client(raw: &str) -> ContentResult<reqwest::Client> {
    let (url, origin) = validate_pack_download_url(raw)?;
    let addresses = resolve_public_pack_addresses(&url, origin.port).await?;
    let redirect_origin = origin.clone();
    let mut builder = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(120))
        .no_proxy()
        .redirect(reqwest::redirect::Policy::custom(move |attempt| {
            if attempt.previous().len() >= MAX_PACK_REDIRECTS {
                return attempt.error("modpack download redirected too many times");
            }
            if pack_redirect_allowed(&redirect_origin, attempt.url()) {
                attempt.follow()
            } else {
                attempt.error("modpack download redirect was not safe")
            }
        }));
    if matches!(url.host(), Some(Host::Domain(_))) {
        builder = builder.resolve_to_addrs(&origin.host, &addresses);
    }
    builder.build().map_err(ContentError::Request)
}

async fn resolve_public_pack_addresses(url: &Url, port: u16) -> ContentResult<Vec<SocketAddr>> {
    let addresses: Vec<SocketAddr> = match url.host() {
        Some(Host::Ipv4(address)) => vec![SocketAddr::new(IpAddr::V4(address), port)],
        Some(Host::Ipv6(address)) => vec![SocketAddr::new(IpAddr::V6(address), port)],
        Some(Host::Domain(domain)) => tokio::net::lookup_host((domain, port))
            .await
            .map_err(|_| {
                ContentError::DownloadPreparation(
                    "modpack download destination could not be resolved".to_string(),
                )
            })?
            .collect(),
        None => Vec::new(),
    };
    if addresses.is_empty() || addresses.iter().any(|address| !is_public_ip(address.ip())) {
        return Err(ContentError::ProviderMetadataInvalid(
            "modpack download destination is not public".to_string(),
        ));
    }
    Ok(addresses)
}

fn pack_redirect_allowed(origin: &PackDownloadOrigin, destination: &Url) -> bool {
    validate_pack_download_url(destination.as_str())
        .is_ok_and(|(_, destination_origin)| destination_origin == *origin)
}

fn is_public_ip(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => is_public_ipv4(address),
        IpAddr::V6(address) => is_public_ipv6(address),
    }
}

fn is_public_ipv4(address: Ipv4Addr) -> bool {
    let [first, second, third, _] = address.octets();
    !(first == 0
        || first == 10
        || first == 127
        || first >= 224
        || (first == 100 && (64..=127).contains(&second))
        || (first == 169 && second == 254)
        || (first == 172 && (16..=31).contains(&second))
        || (first == 192 && second == 168)
        || (first == 192 && second == 0 && matches!(third, 0 | 2))
        || (first == 198 && matches!(second, 18 | 19))
        || (first == 198 && second == 51 && third == 100)
        || (first == 203 && second == 0 && third == 113))
}

fn is_public_ipv6(address: Ipv6Addr) -> bool {
    if let Some(mapped) = address.to_ipv4_mapped() {
        return is_public_ipv4(mapped);
    }
    let segments = address.segments();
    if segments[..6].iter().all(|segment| *segment == 0) {
        let mapped = Ipv4Addr::new(
            (segments[6] >> 8) as u8,
            segments[6] as u8,
            (segments[7] >> 8) as u8,
            segments[7] as u8,
        );
        return is_public_ipv4(mapped);
    }
    !(address.is_unspecified()
        || address.is_loopback()
        || address.is_multicast()
        || (segments[0] & 0xfe00) == 0xfc00
        || (segments[0] & 0xffc0) == 0xfe80
        || (segments[0] & 0xffc0) == 0xfec0
        || (segments[0] == 0x2001 && segments[1] == 0x0db8))
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
                        "a modpack destination is already occupied".to_string(),
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
    pub fn stage_removals(&mut self, removals: &[ManagedRemoval]) -> ContentResult<()> {
        stage_managed_removals(self.transaction, removals)
    }
}

fn apply_overrides(game_dir: &Path, archive: &Path) -> ContentResult<Vec<String>> {
    let file = fs::File::open(archive)?;
    let mut zip = zip::ZipArchive::new(file).map_err(|error| {
        ContentError::ProviderMetadataInvalid(format!("not a readable modpack: {error}"))
    })?;

    let mut applied = Vec::new();
    let mut processed = HashMap::new();
    let mut extracted_files = 0_usize;
    let mut extracted_bytes = 0_u64;
    // Client overrides go last: where both define a file, the client copy wins.
    for root in [OVERRIDES, CLIENT_OVERRIDES] {
        let prefix = format!("{root}/");
        for index in 0..zip.len() {
            let mut entry = zip.by_index(index).map_err(|error| {
                ContentError::ProviderMetadataInvalid(format!("unreadable modpack: {error}"))
            })?;
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
            let relative = normalize_relative_path(&relative)?;
            let first_copy = match processed.get(relative.as_str()) {
                None => {
                    processed.insert(relative.clone(), root);
                    true
                }
                Some(previous_root) if *previous_root == OVERRIDES && root == CLIENT_OVERRIDES => {
                    processed.insert(relative.clone(), root);
                    false
                }
                Some(_) => {
                    return Err(ContentError::ProviderMetadataInvalid(
                        "modpack contains a duplicate override path".to_string(),
                    ));
                }
            };
            if extracted_files >= MAX_OVERRIDE_FILES {
                return Err(ContentError::ProviderMetadataInvalid(
                    "modpack contains too many override files".to_string(),
                ));
            }
            let declared_size = entry.size();
            if declared_size > MAX_OVERRIDE_ENTRY_BYTES
                || extracted_bytes.saturating_add(declared_size) > MAX_OVERRIDE_TOTAL_BYTES
            {
                return Err(ContentError::ProviderMetadataInvalid(
                    "modpack overrides exceed the extraction limit".to_string(),
                ));
            }

            let destination = contained_path(game_dir, &relative)?;
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent)?;
            }
            let mut sink = fs::File::create(&destination)?;
            let copy_limit = MAX_OVERRIDE_ENTRY_BYTES
                .min(MAX_OVERRIDE_TOTAL_BYTES.saturating_sub(extracted_bytes));
            let copied = copy_pack_archive_entry(&mut entry, &mut sink, copy_limit)?;
            extracted_files += 1;
            extracted_bytes = extracted_bytes.saturating_add(copied);
            if first_copy {
                applied.push(relative);
            }
        }
    }
    Ok(applied)
}

fn copy_pack_archive_entry<R, W>(source: &mut R, sink: &mut W, limit: u64) -> ContentResult<u64>
where
    R: Read,
    W: Write,
{
    let mut copied = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let remaining = limit.saturating_sub(copied).saturating_add(1);
        let read_limit = usize::try_from(remaining.min(buffer.len() as u64))
            .expect("bounded override read size fits usize");
        let read = source.read(&mut buffer[..read_limit]).map_err(|_| {
            ContentError::ProviderMetadataInvalid(
                "modpack override entry could not be read".to_string(),
            )
        })?;
        if read == 0 {
            return Ok(copied);
        }
        copied = copied.saturating_add(read as u64);
        if copied > limit {
            return Err(ContentError::ProviderMetadataInvalid(
                "modpack overrides exceed the extraction limit".to_string(),
            ));
        }
        sink.write_all(&buffer[..read])?;
    }
}

/// Resolve `relative` under `root`, refusing anything that would escape it.
fn contained_path(root: &Path, relative: &str) -> ContentResult<PathBuf> {
    let relative = normalize_relative_path(relative)?;
    let mut resolved = root.to_path_buf();
    for part in relative.split('/') {
        resolved.push(part);
    }
    Ok(resolved)
}

/// Canonicalize an archive path lexically so aliases such as `mods/./x.jar`
/// compare equal before any collision or ownership decision is made.
fn normalize_relative_path(relative: &str) -> ContentResult<String> {
    if relative.contains('\\') {
        return Err(ContentError::ProviderMetadataInvalid(format!(
            "modpack file uses a non-portable path: {relative}"
        )));
    }
    let candidate = Path::new(relative);
    if candidate.is_absolute() {
        return Err(ContentError::ProviderMetadataInvalid(format!(
            "modpack file escapes the instance: {relative}"
        )));
    }
    let mut parts = Vec::new();
    for component in candidate.components() {
        match component {
            Component::Normal(part) => parts.push(part.to_string_lossy().into_owned()),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(ContentError::ProviderMetadataInvalid(format!(
                    "modpack file escapes the instance: {relative}"
                )));
            }
        }
    }
    if parts.is_empty() {
        return Err(ContentError::ProviderMetadataInvalid(format!(
            "modpack file path is empty: {relative}"
        )));
    }
    let normalized = parts.join("/");
    if normalized.eq_ignore_ascii_case(MANIFEST_FILE) {
        return Err(ContentError::ProviderMetadataInvalid(format!(
            "modpack file uses a launcher-reserved path: {relative}"
        )));
    }
    Ok(normalized)
}

pub fn parse_pack_index(raw: &str) -> ContentResult<PackIndex> {
    let dto: dto::Index = serde_json::from_str(raw).map_err(|_| {
        ContentError::ProviderMetadataInvalid("modpack index JSON is invalid".to_string())
    })?;
    if dto.format_version > SUPPORTED_FORMAT_VERSION {
        return Err(ContentError::ProviderMetadataInvalid(format!(
            "this modpack needs a newer launcher (format {})",
            dto.format_version
        )));
    }

    let minecraft = dto
        .dependencies
        .get("minecraft")
        .cloned()
        .unwrap_or_default();
    validate_pack_coordinate("Minecraft version", &minecraft)?;

    let loader = loader_from_dependencies(&dto.dependencies)?;
    let files = dto
        .files
        .into_iter()
        .filter(|file| file.included_on_client())
        .map(pack_file)
        .collect::<ContentResult<Vec<PackFile>>>()?;
    let unique_paths: HashSet<&str> = files.iter().map(|file| file.path.as_str()).collect();
    if unique_paths.len() != files.len() {
        return Err(ContentError::ProviderMetadataInvalid(
            "modpack contains duplicate file destinations".to_string(),
        ));
    }

    Ok(PackIndex {
        name: dto.name,
        version: dto.version_id,
        minecraft,
        loader,
        files,
    })
}

fn pack_file(file: dto::IndexFile) -> ContentResult<PackFile> {
    let path = normalize_relative_path(&file.path)?;
    let sha1 = validate_pack_hash(file.hashes.sha1, 40, "sha1", &path)?;
    let sha512 = validate_pack_hash(file.hashes.sha512, 128, "sha512", &path)?;
    if sha1.is_none() && sha512.is_none() {
        return Err(ContentError::ProviderMetadataInvalid(format!(
            "modpack file has no supported integrity hash: {path}"
        )));
    }
    let url = file
        .downloads
        .into_iter()
        .find_map(|raw| {
            validate_pack_download_url(&raw)
                .ok()
                .map(|(url, _)| url.to_string())
        })
        .ok_or_else(|| {
            ContentError::ProviderMetadataInvalid(format!(
                "modpack file has no download: {}",
                file.path
            ))
        })?;
    Ok(PackFile {
        path,
        url,
        sha1,
        sha512,
        size: file.file_size,
    })
}

fn validate_pack_hash(
    hash: Option<String>,
    expected_len: usize,
    algorithm: &str,
    path: &str,
) -> ContentResult<Option<String>> {
    let Some(hash) = hash else {
        return Ok(None);
    };
    if hash.len() != expected_len || !hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(ContentError::ProviderMetadataInvalid(format!(
            "modpack file has an invalid {algorithm} hash: {path}"
        )));
    }
    Ok(Some(hash.to_ascii_lowercase()))
}

fn loader_from_dependencies(
    dependencies: &HashMap<String, String>,
) -> ContentResult<Option<PackLoader>> {
    let loader = [
        ("fabric-loader", LoaderComponentId::Fabric),
        ("quilt-loader", LoaderComponentId::Quilt),
        ("neoforge", LoaderComponentId::NeoForge),
        ("forge", LoaderComponentId::Forge),
    ]
    .into_iter()
    .find_map(|(key, component_id)| {
        dependencies
            .get(key)
            .filter(|version| !version.is_empty())
            .map(|version| PackLoader {
                component_id,
                version: version.clone(),
            })
    });
    if let Some(loader) = loader.as_ref() {
        validate_pack_coordinate("loader version", &loader.version)?;
    }
    Ok(loader)
}

fn validate_pack_coordinate(name: &str, value: &str) -> ContentResult<()> {
    if value.is_empty()
        || value.len() > MAX_PACK_COORDINATE_BYTES
        || value != value.trim()
        || value.chars().any(char::is_control)
    {
        return Err(ContentError::ProviderMetadataInvalid(format!(
            "modpack has an invalid {name}"
        )));
    }
    Ok(())
}

/// The pack's own archive, as a file to download and verify.
pub fn pack_archive_file(file: &FileRef) -> VerifiedContentIntegrity {
    VerifiedContentIntegrity {
        size: file.size,
        sha1: file.sha1.clone(),
        sha512: file.sha512.clone(),
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
                "hashes": { "sha1": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa" },
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
                "hashes": { "sha1": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb" },
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
                component_id: LoaderComponentId::Fabric,
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
    fn pack_paths_are_normalized_before_collision_checks() {
        let raw = r#"{
            "formatVersion": 1,
            "dependencies": { "minecraft": "1.21.6" },
            "files": [{
                "path": "mods/./example.jar",
                "hashes": { "sha1": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa" },
                "downloads": ["https://cdn.modrinth.com/example.jar"]
            }]
        }"#;
        let index = parse_pack_index(raw).expect("normalized index");
        assert_eq!(index.files[0].path, "mods/example.jar");

        let duplicate = r#"{
            "formatVersion": 1,
            "dependencies": { "minecraft": "1.21.6" },
            "files": [
                { "path": "mods/example.jar", "hashes": { "sha1": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa" }, "downloads": ["https://cdn.modrinth.com/a.jar"] },
                { "path": "mods/./example.jar", "hashes": { "sha1": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb" }, "downloads": ["https://cdn.modrinth.com/b.jar"] }
            ]
        }"#;
        assert!(parse_pack_index(duplicate).is_err());
    }

    #[test]
    fn a_pack_without_a_minecraft_version_is_rejected() {
        let raw = r#"{ "formatVersion": 1, "dependencies": {}, "files": [] }"#;
        assert!(parse_pack_index(raw).is_err());
    }

    #[test]
    fn pack_loader_coordinates_are_canonical_before_target_generation() {
        for dependencies in [
            r#"{ "minecraft": " 1.21.6", "fabric-loader": "0.17.2" }"#,
            r#"{ "minecraft": "1.21.6", "fabric-loader": "0.17.2\n" }"#,
            r#"{ "minecraft": "1.21.6", "fabric-loader": " " }"#,
        ] {
            let raw =
                format!(r#"{{ "formatVersion": 1, "dependencies": {dependencies}, "files": [] }}"#);
            assert!(
                matches!(
                    parse_pack_index(&raw),
                    Err(ContentError::ProviderMetadataInvalid(_))
                ),
                "{dependencies}"
            );
        }
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
            "files": [{ "path": "mods/x.jar", "hashes": { "sha1": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa" }, "downloads": ["http://insecure/x.jar"] }]
        }"#;
        assert!(parse_pack_index(raw).is_err());
    }

    #[test]
    fn a_pack_file_without_a_cryptographic_hash_is_rejected() {
        let raw = r#"{
            "formatVersion": 1,
            "dependencies": { "minecraft": "1.21.6" },
            "files": [{
                "path": "mods/x.jar",
                "hashes": {},
                "downloads": ["https://cdn.modrinth.com/x.jar"]
            }]
        }"#;
        assert!(parse_pack_index(raw).is_err());
    }

    #[test]
    fn pack_archive_integrity_preserves_sha512_only_evidence() {
        let file = FileRef {
            url: "https://cdn.modrinth.com/data/project/versions/version/archive.mrpack"
                .to_string(),
            filename: "archive.mrpack".to_string(),
            sha1: None,
            sha512: Some("a".repeat(128)),
            size: Some(42),
            primary: true,
        };

        assert_eq!(
            pack_archive_file(&file),
            VerifiedContentIntegrity {
                size: Some(42),
                sha1: None,
                sha512: Some("a".repeat(128)),
            }
        );
    }

    #[test]
    fn p00_b11_contract_pack_report_replaces_missing_size_with_observed_bytes() {
        let file = PackFile {
            path: "mods/managed.jar".to_string(),
            url: "https://cdn.modrinth.com/managed.jar".to_string(),
            sha1: None,
            sha512: Some("a".repeat(128)),
            size: None,
        };

        let authenticated = authenticated_pack_file(&file, 42);

        assert_eq!(authenticated.size, Some(42));
        assert_eq!(authenticated.sha512, file.sha512);
    }

    #[test]
    fn private_and_special_pack_download_addresses_are_rejected() {
        for address in [
            "127.0.0.1",
            "10.0.0.1",
            "169.254.169.254",
            "172.16.0.1",
            "192.168.1.1",
            "100.64.0.1",
            "[::1]",
            "[fe80::1]",
            "[fec0::1]",
            "[fc00::1]",
            "[::ffff:127.0.0.1]",
        ] {
            assert!(
                validate_pack_download_url(&format!("https://{address}/payload.jar")).is_err(),
                "{address} must not be a pack download destination"
            );
        }
        assert!(validate_pack_download_url("https://1.1.1.1/payload.jar").is_ok());
        assert!(validate_pack_download_url("https://[2606:4700:4700::1111]/payload.jar").is_ok());
    }

    #[test]
    fn pack_redirects_stay_on_the_pinned_public_https_origin() {
        let (_, origin) = validate_pack_download_url("https://downloads.example.com/payload.jar")
            .expect("public HTTPS origin");

        assert!(pack_redirect_allowed(
            &origin,
            &Url::parse("https://downloads.example.com/releases/payload.jar").expect("same origin")
        ));
        for destination in [
            "http://downloads.example.com/payload.jar",
            "https://downloads.example.com:444/payload.jar",
            "https://127.0.0.1/payload.jar",
            "https://169.254.169.254/latest/meta-data",
            "https://cdn.example.com/payload.jar",
        ] {
            assert!(
                !pack_redirect_allowed(&origin, &Url::parse(destination).expect("redirect URL")),
                "redirect must be rejected: {destination}"
            );
        }
    }

    #[tokio::test]
    async fn public_literal_pack_client_builds_without_network_access() {
        build_pack_download_client("https://1.1.1.1/payload.jar")
            .await
            .expect("public address client");
        assert!(
            build_pack_download_client("https://localhost/payload.jar")
                .await
                .is_err()
        );
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
        assert!(matches!(&error, ContentError::ProviderMetadataInvalid(_)));
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
            "mods\\..\\outside.jar",
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

    #[test]
    fn launcher_manifest_paths_are_reserved_at_the_instance_root() {
        let root = Path::new("/instances/aurora");

        for reserved in [
            "axial.content.json",
            "./axial.content.json",
            "AXIAL.CONTENT.JSON",
        ] {
            assert!(
                contained_path(root, reserved).is_err(),
                "{reserved} must remain launcher-owned"
            );
        }
        assert_eq!(
            contained_path(root, "config/axial.content.json").expect("nested path is not reserved"),
            root.join("config").join("axial.content.json")
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
    fn overrides_cannot_claim_launcher_manifest_paths() {
        let root = std::env::temp_dir().join("axial-pack-override-manifest-path");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("root");
        let archive = override_archive(
            "manifest-path",
            &[("overrides/./axial.content.json", b"payload".to_vec())],
        );

        let error = apply_overrides(&root, &archive)
            .expect_err("override must not claim launcher manifest paths");
        assert!(error.to_string().contains("launcher-reserved path"));

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
    fn client_override_replacements_are_reported_once() {
        let root = std::env::temp_dir().join("axial-pack-client-override-replacement");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("root");
        let archive = override_archive(
            "client-replacement",
            &[
                ("overrides/config/shared.bin", vec![b'a'; 128]),
                ("overrides/config/other.bin", vec![b'b'; 128]),
                ("client-overrides/config/shared.bin", vec![b'c'; 128]),
            ],
        );

        let applied = apply_overrides(&root, &archive).expect("apply overrides");
        assert_eq!(applied, ["config/shared.bin", "config/other.bin"]);
        assert_eq!(
            fs::read(root.join("config/shared.bin")).expect("client override"),
            vec![b'c'; 128]
        );

        let _ = fs::remove_file(archive);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn client_override_replacements_count_toward_the_extraction_limit() {
        let root = std::env::temp_dir().join("axial-pack-client-override-limit");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("root");
        let archive = override_archive(
            "client-replacement-limit",
            &[
                (
                    "overrides/config/shared.bin",
                    vec![b'a'; MAX_OVERRIDE_ENTRY_BYTES as usize],
                ),
                (
                    "overrides/config/other.bin",
                    vec![b'b'; MAX_OVERRIDE_ENTRY_BYTES as usize],
                ),
                (
                    "client-overrides/config/shared.bin",
                    b"replacement".to_vec(),
                ),
            ],
        );

        let error = apply_overrides(&root, &archive)
            .expect_err("replacement extraction must remain cumulatively bounded");
        assert!(error.to_string().contains("extraction limit"));

        let _ = fs::remove_file(archive);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn duplicate_override_paths_are_rejected() {
        let root = std::env::temp_dir().join("axial-pack-duplicate-override-path");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("root");
        let archive = override_archive(
            "duplicate-path",
            &[
                ("overrides/config/shared.bin", b"first".to_vec()),
                ("overrides/config/./shared.bin", b"second".to_vec()),
            ],
        );

        let error = apply_overrides(&root, &archive).expect_err("duplicate path must be rejected");
        assert!(matches!(&error, ContentError::ProviderMetadataInvalid(_)));
        assert!(error.to_string().contains("duplicate override path"));

        let _ = fs::remove_file(archive);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn override_paths_return_the_same_normal_form_as_index_paths() {
        let root = std::env::temp_dir().join("axial-pack-override-normalized-path");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("root");
        let archive = override_archive(
            "normalized-path",
            &[("overrides/mods/./example.jar", b"override".to_vec())],
        );

        let applied = apply_overrides(&root, &archive).expect("apply override");
        assert_eq!(applied, ["mods/example.jar"]);
        let indexed = HashSet::from(["mods/example.jar"]);
        assert!(
            applied.iter().any(|path| indexed.contains(path.as_str())),
            "normalized override must collide with the indexed destination"
        );
        assert_eq!(
            std::fs::read(root.join("mods/example.jar")).expect("override file"),
            b"override"
        );

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

    #[tokio::test]
    async fn full_pack_preserves_an_occupied_indexed_destination() {
        let root = std::env::temp_dir().join(format!(
            "axial-pack-full-indexed-occupied-{}-{}",
            std::process::id(),
            crate::transaction::staging_dir(Path::new(""), "test")
                .file_name()
                .expect("sequence")
                .to_string_lossy()
        ));
        fs::create_dir_all(root.join("mods")).expect("mods");
        let destination = root.join("mods/example.jar");
        fs::write(&destination, b"user content").expect("user file");
        let index = br#"{
            "formatVersion": 1,
            "game": "minecraft",
            "versionId": "1.0.0",
            "name": "Test Pack",
            "dependencies": { "minecraft": "1.21.6" },
            "files": [{
                "path": "mods/example.jar",
                "hashes": { "sha1": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa" },
                "downloads": ["https://cdn.modrinth.com/example.jar"]
            }]
        }"#;
        let archive = override_archive("full-indexed-occupied", &[(INDEX_FILE, index.to_vec())]);

        let error = install_pack_files_with_finalize(
            &root,
            &archive,
            PackInstallOptions {
                selected_paths: &[],
                include_overrides: true,
            },
            |_| {},
            |_| {},
            |_, _| Ok(()),
        )
        .await
        .expect_err("full pack must not replace an indexed destination");

        assert!(matches!(&error, ContentError::Invalid(_)));
        assert!(error.to_string().contains("occupied"));
        assert_eq!(
            fs::read(&destination).expect("preserved file"),
            b"user content"
        );
        let _ = fs::remove_file(archive);
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn full_pack_preserves_an_occupied_override_destination() {
        let root = std::env::temp_dir().join(format!(
            "axial-pack-full-override-occupied-{}-{}",
            std::process::id(),
            crate::transaction::staging_dir(Path::new(""), "test")
                .file_name()
                .expect("sequence")
                .to_string_lossy()
        ));
        fs::create_dir_all(root.join("config")).expect("config");
        let destination = root.join("config/options.txt");
        fs::write(&destination, b"user settings").expect("user file");
        let index = br#"{
            "formatVersion": 1,
            "game": "minecraft",
            "versionId": "1.0.0",
            "name": "Test Pack",
            "dependencies": { "minecraft": "1.21.6" },
            "files": []
        }"#;
        let archive = override_archive(
            "full-override-occupied",
            &[
                (INDEX_FILE, index.to_vec()),
                ("overrides/config/options.txt", b"pack settings".to_vec()),
            ],
        );

        let error = install_pack_files_with_finalize(
            &root,
            &archive,
            PackInstallOptions {
                selected_paths: &[],
                include_overrides: true,
            },
            |_| {},
            |_| {},
            |_, _| Ok(()),
        )
        .await
        .expect_err("full pack must not replace an override destination");

        assert!(matches!(&error, ContentError::Invalid(_)));
        assert!(error.to_string().contains("occupied"));
        assert_eq!(
            fs::read(&destination).expect("preserved file"),
            b"user settings"
        );
        let _ = fs::remove_file(archive);
        let _ = fs::remove_dir_all(root);
    }
}
