//! Retained Modrinth pack manifests and authenticated payload planning.
//!
//! An archive is retained by the plan so preview and execution cannot quietly
//! reopen different bytes. Publication belongs to the content mutation owner.

use super::catalog::ContentService;
use super::model::{
    CanonicalContent, CanonicalId, ContentKind, ContentVersion, FileRef, VersionIdentity,
};
use super::resolve::{ContentResolution, ResolutionSelection, ResolutionTarget, pick_version};
use crate::files::portable::{
    PortableFileName, PortableRelativePath, managed_content_name_is_reserved,
};
use crate::network::{
    Checksum, DownloadError, DownloadRequest, HashAlgorithm, IntegrityPolicy, OriginPolicy,
    ProviderClient, ResponseLimits,
};
use crate::tasks::CancellationToken;
use axial_minecraft::loaders::LoaderComponentId;
use serde::{Deserialize, Serialize};
use sha1::Sha1;
use sha2::{Digest, Sha256, Sha512};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{Cursor, Read};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::Arc;
use url::{Host, Url};

const INDEX_FILE: &str = "modrinth.index.json";
const MAX_INDEX_BYTES: u64 = 8 << 20;
pub const MAX_PACK_ARCHIVE_BYTES: u64 = 512 << 20;
pub const MAX_PACK_FILE_BYTES: u64 = 512 << 20;
pub(crate) const MAX_OVERRIDE_ENTRY_BYTES: u64 = 128 << 20;
pub(crate) const MAX_OVERRIDE_TOTAL_BYTES: u64 = 512 << 20;
const MAX_ARCHIVE_ENTRIES: usize = 30_000;
const MAX_INDEX_FILES: usize = 10_000;
const MAX_OVERRIDE_FILES: usize = 10_000;
const MAX_SELECTION_FILES: usize = 500;

#[derive(Debug, thiserror::Error)]
pub enum PackError {
    #[error("modpack metadata is invalid: {0}")]
    Invalid(&'static str),
    #[error("this modpack format is not supported")]
    Unsupported,
    #[error("the modpack exceeds the supported size limit")]
    TooLarge,
    #[error("the selected modpack files changed; review the pack again")]
    SelectionChanged,
    #[error("modpack destinations conflict: {0}")]
    Conflict(String),
    #[error("a modpack payload failed integrity verification")]
    Integrity,
    #[error("modpack staging was cancelled")]
    Cancelled,
    #[error("a modpack download failed")]
    Download,
    #[error("modpack provider metadata could not be resolved")]
    Provider,
}

pub type PackResult<T> = Result<T, PackError>;

fn download_error(error: DownloadError) -> PackError {
    match error {
        DownloadError::Cancelled => PackError::Cancelled,
        DownloadError::ChecksumMismatch | DownloadError::SizeMismatch => PackError::Integrity,
        DownloadError::EncodedLimitExceeded { .. } | DownloadError::DecodedLimitExceeded { .. } => {
            PackError::TooLarge
        }
        _ => PackError::Download,
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct PackLoader {
    pub component_id: LoaderComponentId,
    pub version: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct PackFile {
    pub path: String,
    pub url: String,
    pub sha1: Option<String>,
    pub sha512: Option<String>,
    pub size: Option<u64>,
}

impl PackFile {
    pub fn filename(&self) -> &str {
        self.path.rsplit('/').next().unwrap_or(&self.path)
    }

    /// Nested files are pack payloads, never direct managed mod ownership.
    pub fn kind(&self) -> Option<ContentKind> {
        validate_pack_path(&self.path).ok()?;
        match self.path.rsplit_once('/')?.0 {
            "mods" => Some(ContentKind::Mod),
            "resourcepacks" => Some(ContentKind::ResourcePack),
            "shaderpacks" => Some(ContentKind::ShaderPack),
            _ => None,
        }
    }

    pub fn verify(&self, bytes: &[u8]) -> PackResult<()> {
        verify_payload(
            bytes,
            self.size,
            self.sha1.as_deref(),
            self.sha512.as_deref(),
        )
    }

    /// Complete older pack metadata one bounded file at a time. The eventual
    /// native stream verifies these exact bytes again before publication.
    pub(crate) async fn authenticated_file(
        &self,
        client: &ProviderClient,
        cancellation: &CancellationToken,
    ) -> PackResult<FileRef> {
        let (size, sha512) = match (self.size, self.sha512.as_ref()) {
            (Some(size), Some(sha512)) => (size, sha512.clone()),
            _ => {
                let url = validate_download_url(&self.url)?;
                let (algorithm, hash) = match (self.sha512.as_deref(), self.sha1.as_deref()) {
                    (Some(hash), _) => (HashAlgorithm::Sha512, hash),
                    (_, Some(hash)) => (HashAlgorithm::Sha1, hash),
                    _ => return Err(PackError::Integrity),
                };
                let bytes = client
                    .fetch_public(
                        DownloadRequest::get(
                            self.url.clone(),
                            OriginPolicy::https([url.origin().ascii_serialization()], 3)
                                .map_err(|_| PackError::Download)?,
                            ResponseLimits::new(MAX_PACK_FILE_BYTES, MAX_PACK_FILE_BYTES),
                            IntegrityPolicy::Checksum {
                                checksum: Checksum::from_hex(algorithm, hash)
                                    .map_err(|_| PackError::Integrity)?,
                                expected_size: self.size,
                            },
                        ),
                        cancellation,
                    )
                    .await
                    .map_err(download_error)?
                    .into_bytes();
                self.verify(&bytes)?;
                (bytes.len() as u64, format!("{:x}", Sha512::digest(&bytes)))
            }
        };
        Ok(FileRef {
            filename: self.filename().into(),
            url: self.url.clone(),
            size: Some(size),
            sha512: Some(sha512),
            sha1: self.sha1.clone(),
            primary: true,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct PackIndex {
    pub name: String,
    pub version: String,
    pub minecraft: String,
    pub loader: Option<PackLoader>,
    pub files: Vec<PackFile>,
}

#[derive(Clone, Debug)]
struct PackOverride {
    path: String,
    archive_index: usize,
    archive_path: String,
    size: u64,
    sha512: String,
}

/// The retained bytes confer read authority only. No caller path is retained.
#[derive(Clone, Debug)]
pub struct PackArchive {
    bytes: Arc<[u8]>,
    fingerprint: String,
    index: PackIndex,
    overrides: Vec<PackOverride>,
}

impl PackArchive {
    pub fn read(bytes: impl Into<Arc<[u8]>>) -> PackResult<Self> {
        let bytes = bytes.into();
        if bytes.len() as u64 > MAX_PACK_ARCHIVE_BYTES {
            return Err(PackError::TooLarge);
        }
        let mut zip = zip::ZipArchive::new(Cursor::new(bytes.as_ref()))
            .map_err(|_| PackError::Invalid("not a readable zip archive"))?;
        if zip.len() > MAX_ARCHIVE_ENTRIES {
            return Err(PackError::TooLarge);
        }
        let mut index_position = None;
        for position in 0..zip.len() {
            let entry = zip
                .by_index(position)
                .map_err(|_| PackError::Invalid("unreadable archive entry"))?;
            if entry.name() == INDEX_FILE {
                if index_position.replace(position).is_some() || !regular_entry(&entry) {
                    return Err(PackError::Invalid("duplicate or nonregular pack index"));
                }
            }
        }
        let index_position = index_position.ok_or(PackError::Unsupported)?;
        let raw = read_entry(&mut zip, index_position, MAX_INDEX_BYTES)?;
        let index = parse_pack_index(
            std::str::from_utf8(&raw).map_err(|_| PackError::Invalid("index is not UTF-8"))?,
        )?;
        let overrides = inspect_overrides(&mut zip)?;
        let fingerprint = format!("{:x}", Sha256::digest(bytes.as_ref()));
        drop(zip);
        Ok(Self {
            bytes,
            fingerprint,
            index,
            overrides,
        })
    }

    pub fn index(&self) -> &PackIndex {
        &self.index
    }

    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    /// Full imports may include overrides. Selected imports must be created
    /// from a reviewed file preview and cannot include arbitrary config files.
    pub fn plan_all(&self, include_overrides: bool) -> PackResult<PackPlan> {
        self.plan(self.index.files.clone(), include_overrides)
    }

    fn plan(&self, files: Vec<PackFile>, include_overrides: bool) -> PackResult<PackPlan> {
        let overrides = if include_overrides {
            self.overrides.clone()
        } else {
            Vec::new()
        };
        let destinations = files
            .iter()
            .map(|f| f.path.as_str())
            .chain(overrides.iter().map(|f| f.path.as_str()))
            .collect::<Vec<_>>();
        validate_destinations(destinations)?;
        Ok(PackPlan {
            archive: self.clone(),
            files,
            overrides,
        })
    }

    fn override_bytes(&self, source: &PackOverride) -> PackResult<Vec<u8>> {
        let mut zip = zip::ZipArchive::new(Cursor::new(self.bytes.as_ref()))
            .map_err(|_| PackError::Invalid("retained archive became unreadable"))?;
        {
            let entry = zip
                .by_index(source.archive_index)
                .map_err(|_| PackError::Invalid("override entry is missing"))?;
            if entry.name() != source.archive_path || entry.size() != source.size {
                return Err(PackError::Integrity);
            }
        }
        let bytes = read_entry(&mut zip, source.archive_index, MAX_OVERRIDE_ENTRY_BYTES)?;
        verify_payload(&bytes, Some(source.size), None, Some(&source.sha512))?;
        Ok(bytes)
    }
}

#[derive(Clone, Debug)]
pub struct PackPlan {
    archive: PackArchive,
    files: Vec<PackFile>,
    overrides: Vec<PackOverride>,
}

/// A provider version and its authenticated, retained archive. Provider version
/// IDs stay separate from the pack's own display version in its index.
#[derive(Clone, Debug)]
pub struct ResolvedPack {
    pub canonical_id: CanonicalId,
    pub version_id: String,
    pub name: String,
    pub archive: PackArchive,
}

/// Queue admission resolves only bounded provider metadata. Archive transfer
/// and authenticated planning remain accepted work, not request-owned effects.
pub async fn resolve_pack_version(
    service: &ContentService,
    canonical_id: &CanonicalId,
    version_id: Option<&str>,
) -> PackResult<(CanonicalContent, ContentVersion)> {
    let detail = service
        .detail(canonical_id)
        .await
        .map_err(|_| PackError::Provider)?;
    if detail.content.kind != ContentKind::Modpack {
        return Err(PackError::Unsupported);
    }
    // detail() already validates the canonical project and exact version IDs.
    let version = pick_version(&detail.versions, version_id)
        .ok_or(PackError::SelectionChanged)?
        .clone();
    Ok((detail.content, version))
}

/// Resolve and authenticate the archive before deriving loader requirements or
/// selectable files. Provider-authored URLs use public DNS pinning as well as
/// the ordinary bounded transport and checksum contract.
pub async fn resolve_pack(
    service: &ContentService,
    client: &ProviderClient,
    canonical_id: &CanonicalId,
    version_id: Option<&str>,
    cancellation: &CancellationToken,
) -> PackResult<ResolvedPack> {
    let (content, version) = resolve_pack_version(service, canonical_id, version_id).await?;
    let file = version.primary_file().ok_or(PackError::SelectionChanged)?;
    let url = validate_download_url(&file.url)?;
    let size = file.size;
    if size.is_some_and(|size| size == 0 || size > MAX_PACK_ARCHIVE_BYTES) {
        return Err(PackError::TooLarge);
    }
    let (algorithm, digest) = if let Some(digest) = file.sha512.as_deref() {
        (HashAlgorithm::Sha512, digest)
    } else if let Some(digest) = file.sha1.as_deref() {
        (HashAlgorithm::Sha1, digest)
    } else {
        return Err(PackError::Integrity);
    };
    let checksum = Checksum::from_hex(algorithm, digest).map_err(|_| PackError::Integrity)?;
    let origins = OriginPolicy::https([url.origin().ascii_serialization()], 3)
        .map_err(|_| PackError::Download)?;
    let bytes = Box::pin(client.fetch_public(
        DownloadRequest::get(
            file.url.clone(),
            origins,
            ResponseLimits::new(
                MAX_PACK_ARCHIVE_BYTES,
                size.unwrap_or(MAX_PACK_ARCHIVE_BYTES),
            ),
            IntegrityPolicy::Checksum {
                checksum,
                expected_size: size,
            },
        ),
        cancellation,
    ))
    .await
    .map_err(download_error)?
    .into_bytes();
    verify_payload(&bytes, size, file.sha1.as_deref(), file.sha512.as_deref())?;
    let archive = PackArchive::read(bytes)?;
    Ok(ResolvedPack {
        canonical_id: content.canonical_id,
        version_id: version.id.clone(),
        name: content.title,
        archive,
    })
}

#[derive(Clone, Debug, Serialize)]
pub struct ModpackFileOption {
    pub selection_id: String,
    pub filename: String,
    pub kind: ContentKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    pub title: String,
    pub identified: bool,
    pub compatible: bool,
    pub installed: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct ModpackFilesPlan {
    pub canonical_id: CanonicalId,
    pub version_id: String,
    pub name: String,
    pub minecraft: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub loader: Option<String>,
    pub files: Vec<ModpackFileOption>,
}

#[derive(Clone, Debug)]
struct PreviewFile {
    option: ModpackFileOption,
    file: PackFile,
    identity: Option<VersionIdentity>,
}

/// Eligibility and file identity are retained privately; a wire request cannot
/// submit compatibility flags or promote a config path into a selectable mod.
#[derive(Clone, Debug)]
pub struct PackFilePreview {
    pack: ResolvedPack,
    files: Vec<PreviewFile>,
}

impl PackFilePreview {
    pub fn snapshot(&self) -> ModpackFilesPlan {
        let index = self.pack.archive.index();
        ModpackFilesPlan {
            canonical_id: self.pack.canonical_id.clone(),
            version_id: self.pack.version_id.clone(),
            name: index.name.clone(),
            minecraft: index.minecraft.clone(),
            loader: index
                .loader
                .as_ref()
                .map(|l| l.component_id.short_key().to_string()),
            files: self.files.iter().map(|file| file.option.clone()).collect(),
        }
    }

    pub fn select(&self, selection_ids: &[String]) -> PackResult<PackFileSelection> {
        validate_selection_ids(selection_ids)?;
        if selection_ids.is_empty() {
            return Err(PackError::SelectionChanged);
        }
        let mut files = Vec::with_capacity(selection_ids.len());
        let mut selections = Vec::with_capacity(selection_ids.len());
        let mut identities = HashSet::new();
        for selection in selection_ids {
            let file = self
                .files
                .iter()
                .find(|f| &f.option.selection_id == selection)
                .ok_or(PackError::SelectionChanged)?;
            if !file.option.identified || !file.option.compatible || file.option.installed {
                return Err(PackError::SelectionChanged);
            }
            let identity = file.identity.as_ref().ok_or(PackError::SelectionChanged)?;
            let canonical_id = CanonicalId::for_project(identity.provider, &identity.project_id);
            if !identities.insert((canonical_id.clone(), identity.version_id.clone())) {
                return Err(PackError::SelectionChanged);
            }
            selections.push(ResolutionSelection {
                canonical_id: canonical_id.as_str().to_string(),
                version_id: Some(identity.version_id.clone()),
                kind: file.option.kind,
            });
            files.push(file.file.clone());
        }
        Ok(PackFileSelection {
            plan: self.pack.archive.plan(files, false)?,
            selections,
        })
    }
}

/// Requires the actual dependency resolver result before exposing an executable
/// selected-files plan. Missing dependencies are a conflict, never skipped.
pub struct PackFileSelection {
    plan: PackPlan,
    selections: Vec<ResolutionSelection>,
}

impl PackFileSelection {
    pub fn selections(&self) -> &[ResolutionSelection] {
        &self.selections
    }

    pub fn finish(self, resolution: &ContentResolution) -> PackResult<PackPlan> {
        if !resolution.conflicts.is_empty() {
            return Err(PackError::SelectionChanged);
        }
        let expected = self
            .selections
            .iter()
            .map(|s| {
                (
                    s.canonical_id.as_str(),
                    s.version_id.as_deref().expect("selected version is exact"),
                )
            })
            .collect::<HashSet<_>>();
        let mut matched = HashSet::new();
        for item in &resolution.items {
            let identity = (item.canonical_id.as_str(), item.version_id.as_str());
            if expected.contains(&identity) {
                // The hash lookup identifies this exact archive member, not an
                // arbitrary primary file from the same provider version.
                let selected = self
                    .selections
                    .iter()
                    .zip(&self.plan.files)
                    .find(|(selection, _)| {
                        selection.canonical_id == item.canonical_id.as_str()
                            && selection.version_id.as_deref() == Some(item.version_id.as_str())
                    })
                    .map(|(_, file)| file);
                if !selected.is_some_and(|file| {
                    file.kind() == Some(item.kind)
                        && file.filename() == item.file.filename
                        && file.sha512.is_some()
                        && file.sha512 == item.file.sha512
                        && file.size == item.file.size
                }) {
                    return Err(PackError::SelectionChanged);
                }
                matched.insert(identity);
            } else if !item.already_installed || item.update {
                return Err(PackError::SelectionChanged);
            }
        }
        if matched != expected {
            return Err(PackError::SelectionChanged);
        }
        Ok(self.plan)
    }
}

/// `occupied_paths` comes from the admitted instance inventory. It includes
/// enabled, disabled and alias variants; missing ownership never means absent.
pub async fn preview_files(
    service: &ContentService,
    pack: ResolvedPack,
    target: &ResolutionTarget,
    occupied_paths: &[String],
) -> PackResult<PackFilePreview> {
    let index = pack.archive.index();
    if index.files.iter().filter(|f| f.kind().is_some()).count() > MAX_SELECTION_FILES {
        return Err(PackError::TooLarge);
    }
    let mut hashes = index
        .files
        .iter()
        .filter(|file| file.kind().is_some())
        .filter_map(|f| f.sha512.clone())
        .collect::<Vec<_>>();
    hashes.sort();
    hashes.dedup();
    let identities = service
        .identify(&hashes)
        .await
        .map_err(|_| PackError::Provider)?;
    let mut projects = identities
        .values()
        .map(|identity| CanonicalId::for_project(identity.provider, &identity.project_id))
        .collect::<Vec<_>>();
    projects.sort_by(|a, b| a.as_str().cmp(b.as_str()));
    projects.dedup();
    let metadata = service
        .metadata(&projects)
        .await
        .map_err(|_| PackError::Provider)?;
    let occupied = occupied_paths
        .iter()
        .map(|path| {
            let path =
                PortableRelativePath::new_exact(path).map_err(|_| PackError::SelectionChanged)?;
            let key = path.key().as_str().to_string();
            Ok(key.strip_suffix(".disabled").unwrap_or(&key).to_string())
        })
        .collect::<PackResult<HashSet<_>>>()?;
    let mut files = Vec::new();
    for file in &index.files {
        let Some(kind) = file.kind() else { continue };
        let identity = file.sha512.as_ref().and_then(|hash| identities.get(hash));
        let project = identity
            .map(|identity| CanonicalId::for_project(identity.provider, &identity.project_id));
        let metadata = project.as_ref().and_then(|project| metadata.get(project));
        let compatible = identity.is_some_and(|identity| {
            identity.game_versions.contains(&target.game_version)
                && (kind != ContentKind::Mod
                    || (target.supports_mods && identity.loaders.contains(&target.loader)))
                && metadata.is_some_and(|metadata| metadata.kind == kind)
        });
        let filename = bounded_text(file.filename(), "Pack file");
        let title = metadata
            .map(|metadata| metadata.title.as_str())
            .or_else(|| identity.and_then(|identity| identity.title.as_deref()))
            .unwrap_or(&filename);
        files.push(PreviewFile {
            option: ModpackFileOption {
                selection_id: file_selection_id(
                    &pack.canonical_id,
                    &pack.version_id,
                    &file.path,
                    pack.archive.fingerprint(),
                ),
                filename: filename.clone(),
                kind,
                size: file.size,
                title: bounded_text(title, "Pack file"),
                identified: identity.is_some(),
                compatible,
                installed: occupied.contains(&path_key(&file.path)),
            },
            file: file.clone(),
            identity: identity.cloned(),
        });
    }
    files.sort_by_key(|file| file.option.title.to_lowercase());
    Ok(PackFilePreview { pack, files })
}

pub fn file_selection_id(
    pack: &CanonicalId,
    version: &str,
    path: &str,
    archive_fingerprint: &str,
) -> String {
    let mut digest = Sha256::new();
    for value in [
        b"axial.modpack-file-selection.v1".as_slice(),
        pack.as_str().as_bytes(),
        version.as_bytes(),
        path.as_bytes(),
        archive_fingerprint.as_bytes(),
    ] {
        digest.update((value.len() as u64).to_be_bytes());
        digest.update(value);
    }
    format!("mpf1-{:x}", digest.finalize())
}

pub fn validate_selection_ids(ids: &[String]) -> PackResult<()> {
    let unique = ids.iter().collect::<HashSet<_>>();
    if ids.len() > MAX_SELECTION_FILES
        || unique.len() != ids.len()
        || ids.iter().any(|id| {
            id.len() != 69
                || !id.strip_prefix("mpf1-").is_some_and(|digest| {
                    digest
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                })
        })
    {
        return Err(PackError::SelectionChanged);
    }
    Ok(())
}

impl PackPlan {
    pub fn index(&self) -> &PackIndex {
        self.archive.index()
    }
    pub fn fingerprint(&self) -> &str {
        self.archive.fingerprint()
    }
    pub fn files(&self) -> &[PackFile] {
        &self.files
    }
    pub fn override_count(&self) -> usize {
        self.overrides.len()
    }
    pub fn destinations(&self) -> Vec<&str> {
        self.files
            .iter()
            .map(|f| f.path.as_str())
            .chain(self.overrides.iter().map(|f| f.path.as_str()))
            .collect()
    }

    pub(crate) fn overrides(&self) -> impl Iterator<Item = (&str, u64, &str)> {
        self.overrides
            .iter()
            .map(|source| (source.path.as_str(), source.size, source.sha512.as_str()))
    }

    pub(crate) fn override_bytes(&self, path: &str) -> PackResult<Vec<u8>> {
        let source = self
            .overrides
            .iter()
            .find(|source| source.path == path)
            .ok_or(PackError::SelectionChanged)?;
        self.archive.override_bytes(source)
    }
}

pub fn parse_pack_index(raw: &str) -> PackResult<PackIndex> {
    if raw.len() as u64 > MAX_INDEX_BYTES {
        return Err(PackError::TooLarge);
    }
    let dto: dto::Index =
        serde_json::from_str(raw).map_err(|_| PackError::Invalid("index JSON is invalid"))?;
    if dto.format_version > 1 || dto.game.as_deref().is_some_and(|g| g != "minecraft") {
        return Err(PackError::Unsupported);
    }
    let minecraft = dto
        .dependencies
        .get("minecraft")
        .ok_or(PackError::Invalid("Minecraft version is missing"))?
        .clone();
    validate_coordinate(&minecraft)?;
    let mut loader = None;
    for (name, kind) in [
        ("fabric-loader", LoaderComponentId::Fabric),
        ("quilt-loader", LoaderComponentId::Quilt),
        ("neoforge", LoaderComponentId::NeoForge),
        ("forge", LoaderComponentId::Forge),
    ] {
        if let Some(version) = dto.dependencies.get(name) {
            validate_coordinate(version)?;
            if loader
                .replace(PackLoader {
                    component_id: kind,
                    version: version.clone(),
                })
                .is_some()
            {
                return Err(PackError::Invalid("multiple loader declarations"));
            }
        }
    }
    // Unknown loader declarations cannot silently become Vanilla.
    if dto.dependencies.keys().any(|key| {
        !matches!(
            key.as_str(),
            "minecraft" | "fabric-loader" | "quilt-loader" | "neoforge" | "forge"
        )
    }) {
        return Err(PackError::Unsupported);
    }
    if dto.files.len() > MAX_INDEX_FILES {
        return Err(PackError::TooLarge);
    }
    let mut files = Vec::new();
    for file in dto.files {
        if file.env.as_ref().and_then(|e| e.client.as_deref()) == Some("unsupported") {
            continue;
        }
        validate_pack_path(&file.path)?;
        let sha1 = validate_hash(file.hashes.sha1, 40)?;
        let sha512 = validate_hash(file.hashes.sha512, 128)?;
        if sha1.is_none() && sha512.is_none() {
            return Err(PackError::Invalid("file has no integrity hash"));
        }
        if file
            .file_size
            .is_some_and(|size| size > MAX_PACK_FILE_BYTES)
        {
            return Err(PackError::TooLarge);
        }
        let url = file
            .downloads
            .iter()
            .find_map(|url| validate_download_url(url).ok())
            .ok_or(PackError::Invalid("file has no public HTTPS download"))?
            .to_string();
        files.push(PackFile {
            path: file.path,
            url,
            sha1,
            sha512,
            size: file.file_size,
        });
    }
    validate_destinations(files.iter().map(|file| file.path.as_str()))?;
    Ok(PackIndex {
        name: bounded_text(&dto.name, "Modpack"),
        version: dto.version_id,
        minecraft,
        loader,
        files,
    })
}

fn inspect_overrides<R: Read + std::io::Seek>(
    zip: &mut zip::ZipArchive<R>,
) -> PackResult<Vec<PackOverride>> {
    let mut overrides = Vec::<PackOverride>::new();
    let mut positions = HashMap::<String, (usize, &str)>::new();
    let mut total_bytes = 0u64;
    let mut total_files = 0usize;
    for root in ["overrides", "client-overrides"] {
        let prefix = format!("{root}/");
        for archive_index in 0..zip.len() {
            let entry = zip
                .by_index(archive_index)
                .map_err(|_| PackError::Invalid("unreadable override entry"))?;
            if !entry.name().starts_with(&prefix) {
                continue;
            }
            let path = entry.name()[prefix.len()..].to_string();
            if entry.is_dir() {
                validate_pack_path(path.trim_end_matches('/'))?;
                continue;
            }
            if !regular_entry(&entry) {
                return Err(PackError::Invalid("override is not a regular file"));
            }
            validate_pack_path(&path)?;
            let archive_path = entry.name().to_string();
            let declared_size = entry.size();
            drop(entry);
            total_files += 1;
            if total_files > MAX_OVERRIDE_FILES
                || declared_size > MAX_OVERRIDE_ENTRY_BYTES
                || total_bytes.saturating_add(declared_size) > MAX_OVERRIDE_TOTAL_BYTES
            {
                return Err(PackError::TooLarge);
            }
            let bytes = read_entry(
                zip,
                archive_index,
                MAX_OVERRIDE_ENTRY_BYTES.min(MAX_OVERRIDE_TOTAL_BYTES - total_bytes),
            )?;
            total_bytes += bytes.len() as u64;
            let key = path_key(&path);
            let position = match positions.get(&key) {
                None => overrides.len(),
                Some((position, previous))
                    if *previous == "overrides"
                        && root == "client-overrides"
                        && overrides[*position].path == path =>
                {
                    *position
                }
                Some(_) => return Err(PackError::Conflict(path)),
            };
            positions.insert(key, (position, root));
            let source = PackOverride {
                path,
                archive_index,
                archive_path,
                size: bytes.len() as u64,
                sha512: format!("{:x}", Sha512::digest(&bytes)),
            };
            if position == overrides.len() {
                overrides.push(source);
            } else {
                overrides[position] = source;
            }
        }
    }
    validate_destinations(overrides.iter().map(|source| source.path.as_str()))?;
    Ok(overrides)
}

fn regular_entry(entry: &zip::read::ZipFile<'_>) -> bool {
    !entry.is_dir()
        && entry
            .unix_mode()
            .is_none_or(|mode| matches!(mode & 0o170000, 0 | 0o100000))
}

fn read_entry<R: Read + std::io::Seek>(
    zip: &mut zip::ZipArchive<R>,
    position: usize,
    limit: u64,
) -> PackResult<Vec<u8>> {
    let mut entry = zip
        .by_index(position)
        .map_err(|_| PackError::Invalid("archive entry is missing"))?;
    if !regular_entry(&entry) {
        return Err(PackError::Invalid("archive entry is not a regular file"));
    }
    if entry.size() > limit {
        return Err(PackError::TooLarge);
    }
    let declared = entry.size();
    let mut bytes = Vec::new();
    (&mut entry)
        .take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| PackError::Invalid("archive entry could not be read"))?;
    if bytes.len() as u64 > limit {
        return Err(PackError::TooLarge);
    }
    if bytes.len() as u64 != declared {
        return Err(PackError::Integrity);
    }
    Ok(bytes)
}

fn validate_hash(value: Option<String>, len: usize) -> PackResult<Option<String>> {
    let Some(value) = value else { return Ok(None) };
    if value.len() != len || !value.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(PackError::Invalid("integrity hash is invalid"));
    }
    Ok(Some(value.to_ascii_lowercase()))
}

fn verify_payload(
    bytes: &[u8],
    size: Option<u64>,
    sha1: Option<&str>,
    sha512: Option<&str>,
) -> PackResult<()> {
    if bytes.len() as u64 > MAX_PACK_FILE_BYTES {
        return Err(PackError::TooLarge);
    }
    if size.is_some_and(|size| size != bytes.len() as u64)
        || (sha1.is_none() && sha512.is_none())
        || sha1.is_some_and(|hash| format!("{:x}", Sha1::digest(bytes)) != hash)
        || sha512.is_some_and(|hash| format!("{:x}", Sha512::digest(bytes)) != hash)
    {
        return Err(PackError::Integrity);
    }
    Ok(())
}

fn validate_coordinate(value: &str) -> PackResult<()> {
    if value.is_empty()
        || value.len() > 512
        || value != value.trim()
        || value.chars().any(char::is_control)
    {
        return Err(PackError::Invalid(
            "loader or Minecraft coordinate is invalid",
        ));
    }
    Ok(())
}

pub(crate) fn validate_pack_path(value: &str) -> PackResult<PortableRelativePath> {
    let path = PortableRelativePath::new_exact(value)
        .map_err(|_| PackError::Invalid("file uses an invalid portable path"))?;
    if value.split('/').count() > 64 {
        return Err(PackError::Invalid("file path is too deeply nested"));
    }
    let mut components = value.split('/');
    let first = components.next().expect("portable path is nonempty");
    let first_key = PortableFileName::new_exact(first)
        .expect("portable component")
        .key();
    let managed = matches!(first_key.as_str(), "mods" | "resourcepacks" | "shaderpacks");
    if managed && first != first_key.as_str() {
        return Err(PackError::Invalid(
            "managed directory spelling is not canonical",
        ));
    }
    let direct_managed = managed && value.matches('/').count() == 1;
    if direct_managed
        && (path.file_name().key().as_str().ends_with(".disabled")
            || path.file_name().with_suffix(".disabled").is_err())
    {
        return Err(PackError::Invalid("managed file name is reserved"));
    }
    // Every ancestor must remain a directory; internal publication names may
    // never be supplied as an ancestor to a pack destination either.
    for (position, component) in value.split('/').enumerate() {
        if (position == 0 || direct_managed)
            && managed_content_name_is_reserved(
                &PortableFileName::new_exact(component).expect("portable component"),
            )
        {
            return Err(PackError::Invalid("launcher metadata paths are reserved"));
        }
    }
    Ok(path)
}

fn path_key(value: &str) -> String {
    PortableRelativePath::new_exact(value)
        .expect("validated portable path")
        .key()
        .as_str()
        .to_string()
}

pub(crate) fn validate_destinations<'a>(
    destinations: impl IntoIterator<Item = &'a str>,
) -> PackResult<()> {
    let mut keys = BTreeMap::new();
    for destination in destinations {
        validate_pack_path(destination)?;
        let key = path_key(destination);
        if keys.insert(key, destination).is_some() {
            return Err(PackError::Conflict(destination.into()));
        }
    }
    for (key, destination) in &keys {
        let mut parent = key.as_str();
        while let Some((next, _)) = parent.rsplit_once('/') {
            if keys.contains_key(next) {
                return Err(PackError::Conflict((*destination).into()));
            }
            parent = next;
        }
    }
    Ok(())
}

fn bounded_text(value: &str, fallback: &str) -> String {
    let value = value
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(160)
        .collect::<String>();
    if value.is_empty() {
        fallback.into()
    } else {
        value
    }
}

pub fn validate_download_url(raw: &str) -> PackResult<Url> {
    let url = Url::parse(raw).map_err(|_| PackError::Invalid("download URL is invalid"))?;
    if url.scheme() != "https"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err(PackError::Invalid("downloads require a public HTTPS URL"));
    }
    match url.host() {
        Some(Host::Ipv4(ip)) if is_public_ip(IpAddr::V4(ip)) => (),
        Some(Host::Ipv6(ip)) if is_public_ip(IpAddr::V6(ip)) => (),
        Some(Host::Domain(domain))
            if domain != "localhost"
                && !domain.ends_with(".localhost")
                && !domain.ends_with(".local")
                && domain.contains('.') =>
        {
            ()
        }
        _ => return Err(PackError::Invalid("download destination is not public")),
    }
    Ok(url)
}

pub(crate) fn is_public_ip(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => public_ipv4(address),
        IpAddr::V6(address) => public_ipv6(address),
    }
}

fn public_ipv4(address: Ipv4Addr) -> bool {
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

fn public_ipv6(address: Ipv6Addr) -> bool {
    if let Some(mapped) = address.to_ipv4_mapped() {
        return public_ipv4(mapped);
    }
    let s = address.segments();
    // Only ordinary global unicast addresses; exclude local, translation,
    // documentation and transition ranges that can address private networks.
    (s[0] & 0xe000) == 0x2000
        && s[0] != 0x2002
        && !(s[0] == 0x2001 && (s[1] <= 0x01ff || s[1] == 0x0db8))
}

mod dto {
    use super::*;
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    pub struct Index {
        #[serde(default)]
        pub format_version: u32,
        pub game: Option<String>,
        #[serde(default)]
        pub name: String,
        #[serde(default)]
        pub version_id: String,
        #[serde(default)]
        pub dependencies: BTreeMap<String, String>,
        #[serde(default)]
        pub files: Vec<IndexFile>,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    pub struct IndexFile {
        pub path: String,
        #[serde(default)]
        pub hashes: Hashes,
        pub env: Option<Env>,
        #[serde(default)]
        pub downloads: Vec<String>,
        pub file_size: Option<u64>,
    }
    #[derive(Default, Deserialize)]
    pub struct Hashes {
        pub sha1: Option<String>,
        pub sha512: Option<String>,
    }
    #[derive(Deserialize)]
    pub struct Env {
        pub client: Option<String>,
    }
}

#[cfg(test)]
mod tests {
    use super::super::{
        model::{FileRef, ProviderId},
        resolve::{ResolutionReason, ResolvedContentItem},
    };
    use super::*;
    use std::io::Write;

    fn archive(index: serde_json::Value, extras: &[(&str, &[u8])]) -> PackResult<PackArchive> {
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        writer
            .start_file(INDEX_FILE, zip::write::SimpleFileOptions::default())
            .unwrap();
        writer
            .write_all(&serde_json::to_vec(&index).unwrap())
            .unwrap();
        for (name, bytes) in extras {
            writer
                .start_file(*name, zip::write::SimpleFileOptions::default())
                .unwrap();
            writer.write_all(bytes).unwrap();
        }
        PackArchive::read(writer.finish().unwrap().into_inner())
    }

    #[test]
    fn client_override_precedence_is_authenticated_without_adopting_server_files() {
        let archive = archive(
            serde_json::json!({"dependencies":{"minecraft":"1.21.4"}}),
            &[
                ("overrides/config/settings.txt", b"common"),
                ("client-overrides/config/settings.txt", b"client"),
                ("server-overrides/config/server.txt", b"server"),
            ],
        )
        .unwrap();
        let plan = archive.plan_all(true).unwrap();
        assert_eq!(plan.destinations(), vec!["config/settings.txt"]);
        assert_eq!(
            archive.override_bytes(&plan.overrides[0]).unwrap(),
            b"client"
        );
        assert!(archive.plan_all(false).unwrap().destinations().is_empty());
    }

    #[tokio::test]
    async fn complete_file_proofs_need_no_fetch_and_older_metadata_fetch_is_cancellable() {
        let client = ProviderClient::new(crate::network::ClientConfig::default()).unwrap();
        let bytes = b"pack payload";
        let mut file = PackFile {
            path: "config/nested/data.bin".into(),
            url: "https://offline-fixture.invalid/data.bin".into(),
            size: Some(bytes.len() as u64),
            sha512: Some(format!("{:x}", Sha512::digest(bytes))),
            sha1: Some(format!("{:x}", Sha1::digest(bytes))),
        };
        let authenticated = file
            .authenticated_file(&client, &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(authenticated.filename, "data.bin");
        assert_eq!(authenticated.size, file.size);
        assert_eq!(authenticated.sha512, file.sha512);
        file.sha512 = None;
        file.size = None;
        file.verify(bytes).unwrap();
        assert!(matches!(file.verify(b"changed"), Err(PackError::Integrity)));
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert!(matches!(
            file.authenticated_file(&client, &cancel).await,
            Err(PackError::Cancelled)
        ));
    }

    #[test]
    fn pack_archive_rejects_traversal_reserved_and_destination_aliases() {
        for path in [
            "overrides/../outside",
            "overrides/axial.content.json",
            "overrides/mods/CON.jar",
        ] {
            assert!(
                archive(
                    serde_json::json!({"dependencies":{"minecraft":"1.21.4"}}),
                    &[(path, b"untrusted")]
                )
                .is_err(),
                "{path}"
            );
        }
        assert!(
            archive(
                serde_json::json!({"dependencies":{"minecraft":"1.21.4"}}),
                &[
                    ("overrides/config/A.txt", b"one"),
                    ("overrides/config/a.txt", b"two"),
                ]
            )
            .is_err()
        );
    }

    #[test]
    fn selected_archive_member_cannot_be_replaced_by_another_primary_file() {
        let hash = format!("{:x}", Sha512::digest(b"member"));
        let archive = archive(
            serde_json::json!({"dependencies":{"minecraft":"1.21.4"},"files":[{
                "path":"resourcepacks/member.zip", "hashes":{"sha512":hash}, "fileSize":6,
                "downloads":["https://cdn.example.com/member.zip"]
            }]}),
            &[],
        )
        .unwrap();
        let selected = || PackFileSelection {
            plan: archive.plan_all(false).unwrap(),
            selections: vec![ResolutionSelection {
                canonical_id: "modrinth:member".into(),
                version_id: Some("exact".into()),
                kind: ContentKind::ResourcePack,
            }],
        };
        let mut resolution = ContentResolution {
            conflicts: vec![],
            items: vec![ResolvedContentItem {
                canonical_id: CanonicalId("modrinth:member".into()),
                provider: ProviderId::Modrinth,
                project_id: "member".into(),
                kind: ContentKind::ResourcePack,
                version_id: "exact".into(),
                version_number: "exact".into(),
                title: "Member".into(),
                dependencies: vec![],
                reason: ResolutionReason::Selected,
                already_installed: false,
                update: false,
                file: FileRef {
                    url: "https://cdn.example.com/member.zip".into(),
                    filename: "member.zip".into(),
                    sha512: Some(hash),
                    sha1: None,
                    size: Some(6),
                    primary: true,
                },
            }],
        };
        assert!(selected().finish(&resolution).is_ok());
        resolution.items[0].file.sha512 = Some("a".repeat(128));
        assert!(matches!(
            selected().finish(&resolution),
            Err(PackError::SelectionChanged)
        ));
    }

    #[test]
    fn selected_file_ids_bind_the_archive_bytes_not_only_a_provider_version_label() {
        let pack = CanonicalId("modrinth:pack".into());
        let first = file_selection_id(&pack, "v1", "mods/a.jar", &"a".repeat(64));
        let changed = file_selection_id(&pack, "v1", "mods/a.jar", &"b".repeat(64));
        assert_ne!(first, changed);
        validate_selection_ids(&[first]).unwrap();
    }
}
