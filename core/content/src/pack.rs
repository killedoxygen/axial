//! Modrinth modpack (`.mrpack`) import. A pack is a zip holding an index of
//! files to fetch plus an `overrides/` tree to copy in verbatim. It is not
//! content you add to an instance — it *is* an instance, so this materializes
//! one rather than dropping a file in a folder.
//!
//! Every path out of the archive is untrusted. A pack that names
//! `../../../.ssh/authorized_keys` must not be able to write there, so both the
//! indexed downloads and the overrides go through the same containment check.

use crate::error::{ContentError, ContentResult};
use crate::managed_transaction::{
    ManagedContentExecutionPlan, ManagedContentOperationProjection, ManagedPackPayloadSource,
    ManagedPackProjectedPayload, plan_managed_pack_transaction,
};
use crate::model::{ContentKind, ManagedContentFileName};
use crate::transaction::{ManagedContentInventory, managed_content_parent};
use axial_minecraft::LoaderComponentId;
use axial_minecraft::download::{
    ExpectedTransferDigests, MAX_MANAGED_TRANSFER_BYTES, TransferCancellation, TransferContract,
};
use axial_minecraft::managed_path::{
    ManagedContentIssuedTransfer, ManagedContentPayloadId, ManagedContentPlanningSession,
    ManagedContentTransactionSession, ManagedContentTransferSettlement,
};
use axial_minecraft::portable_path::{
    PortablePathKey, PortableRelativePath, managed_content_name_is_reserved,
    managed_content_name_key,
};
use serde::Deserialize;
use sha2::{Digest as _, Sha512};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{Read, Seek, SeekFrom};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::num::NonZeroU64;
use std::path::Path;
#[cfg(test)]
use std::{fs, path::PathBuf};
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
const MAX_PACK_COORDINATE_BYTES: usize = 512;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct PackDownloadOrigin {
    host: String,
    port: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct PackDestinationKey {
    parent: Option<PortablePathKey>,
    name: PortablePathKey,
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
        let path = PortableRelativePath::new_exact(&self.path).ok()?;
        let parent = managed_content_parent(portable_parent(&path).as_ref())
            .ok()
            .flatten()?;
        ManagedContentFileName::new_exact(path.file_name().as_str()).ok()?;
        Some(parent.kind())
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

/// One bounded, alias-aware view of managed pack destinations. Callers can
/// classify every preview file without reopening or rediscovering paths.
#[derive(Debug, Clone)]
pub struct ManagedPackAvailability {
    occupied: HashSet<PortablePathKey>,
}

impl ManagedPackAvailability {
    pub fn capture(game_dir: &Path, files: &[PackFile]) -> ContentResult<Self> {
        let mut candidates = Vec::new();
        let mut guarded_paths = Vec::new();
        for file in files {
            let path = normalize_relative_path(&file.path)?;
            let Some(kind) = managed_content_parent(portable_parent(&path).as_ref())?
                .map(|parent| parent.kind())
            else {
                continue;
            };
            let filename =
                ManagedContentFileName::new_exact(path.file_name().as_str()).map_err(|_| {
                    ContentError::ProviderMetadataInvalid(
                        "modpack file uses a launcher-reserved or non-canonical path".to_string(),
                    )
                })?;
            let parent = kind
                .install_subdir()
                .expect("managed pack file kinds have install directories");
            let enabled = format!("{parent}/{}", filename.as_str());
            if enabled != path.as_str() {
                return Err(ContentError::ProviderMetadataInvalid(
                    "modpack file uses a launcher-reserved or non-canonical path".to_string(),
                ));
            }
            let disabled = format!("{parent}/{}", filename.disabled().as_str());
            guarded_paths.push(enabled.clone());
            guarded_paths.push(disabled.clone());
            candidates.push((path.key(), enabled, disabled));
        }
        guarded_paths.sort();
        guarded_paths.dedup();
        let inventory = ManagedContentInventory::capture(game_dir, &guarded_paths)?;
        let mut occupied = HashSet::with_capacity(candidates.len());
        for (key, enabled, disabled) in candidates {
            if inventory.require_exact_managed_file_variant_or_absent(&enabled, &disabled)? {
                occupied.insert(key);
            }
        }
        Ok(Self { occupied })
    }

    pub fn contains(&self, file: &PackFile) -> bool {
        normalize_relative_path(&file.path).is_ok_and(|path| self.occupied.contains(&path.key()))
    }
}

#[derive(Debug, Clone, Copy)]
pub struct PackInstallOptions<'a> {
    pub selected_paths: &'a [String],
    pub additional_guarded_paths: &'a [String],
    pub include_overrides: bool,
}

#[derive(Debug)]
struct ManagedPackOverride {
    id: ManagedContentPayloadId,
    archive_index: usize,
    archive_path: String,
    path: PortableRelativePath,
    size: u64,
    sha512: [u8; 64],
}

pub struct ManagedPackOverrideSource {
    id: ManagedContentPayloadId,
    archive_index: usize,
    archive_path: String,
    size: u64,
}

impl ManagedPackOverrideSource {
    pub fn id(&self) -> &ManagedContentPayloadId {
        &self.id
    }
}

struct ManagedPackReportMember {
    id: ManagedContentPayloadId,
    indexed: Option<PackFile>,
    exact_size: Option<u64>,
}

pub struct ManagedPackReportPlan {
    index: PackIndex,
    members: Vec<ManagedPackReportMember>,
    overrides_applied: usize,
}

impl ManagedPackReportPlan {
    pub fn finish(
        self,
        reports: Vec<(ManagedContentPayloadId, u64)>,
    ) -> ContentResult<PackInstallReport> {
        let mut reports = reports.into_iter().collect::<BTreeMap<_, _>>();
        if reports.len() != self.members.len() {
            return Err(ContentError::Invalid(
                "modpack transfer reports are incomplete or duplicated".to_string(),
            ));
        }
        let mut installed = Vec::new();
        for member in self.members {
            let bytes = reports.remove(&member.id).ok_or_else(|| {
                ContentError::Invalid("modpack transfer report identity changed".to_string())
            })?;
            if member.exact_size.is_some_and(|expected| expected != bytes) {
                return Err(ContentError::Invalid(
                    "modpack transfer report size changed".to_string(),
                ));
            }
            if let Some(file) = member.indexed {
                installed.push(authenticated_pack_file(&file, bytes));
            }
        }
        if !reports.is_empty() {
            return Err(ContentError::Invalid(
                "modpack transfer reports contain an unknown identity".to_string(),
            ));
        }
        Ok(PackInstallReport {
            index: self.index,
            installed,
            overrides_applied: self.overrides_applied,
        })
    }
}

pub struct ManagedPackPlan {
    index: PackIndex,
    indexed: Vec<(ManagedContentPayloadId, PackFile, TransferContract)>,
    overrides: Vec<ManagedPackOverride>,
    removal_paths: Vec<PortableRelativePath>,
}

impl ManagedPackPlan {
    pub fn observation_paths(&self) -> Vec<PortableRelativePath> {
        self.indexed
            .iter()
            .map(|(_, file, _)| {
                PortableRelativePath::new_exact(&file.path)
                    .expect("validated pack file path remains portable")
            })
            .chain(self.overrides.iter().map(|source| source.path.clone()))
            .chain(self.removal_paths.iter().cloned())
            .collect()
    }

    pub fn project(
        self,
        session: &ManagedContentPlanningSession,
    ) -> ContentResult<ManagedPackTransactionProjection> {
        let mut payloads = Vec::with_capacity(self.indexed.len() + self.overrides.len());
        let mut report_members = Vec::with_capacity(payloads.capacity());
        for (id, file, contract) in self.indexed {
            payloads.push(ManagedPackProjectedPayload {
                id: id.clone(),
                path: PortableRelativePath::new_exact(&file.path)
                    .expect("validated pack file path remains portable"),
                contract,
                source: ManagedPackPayloadSource::Remote {
                    url: Url::parse(&file.url).expect("validated pack URL remains valid"),
                    display_name: file.filename().to_string(),
                },
            });
            report_members.push(ManagedPackReportMember {
                id,
                exact_size: file.size,
                indexed: Some(file),
            });
        }
        let mut override_sources = Vec::with_capacity(self.overrides.len());
        for source in self.overrides {
            let contract = exact_or_empty_contract(
                source.size,
                ExpectedTransferDigests::sha512(source.sha512),
            )?;
            payloads.push(ManagedPackProjectedPayload {
                id: source.id.clone(),
                path: source.path,
                contract,
                source: ManagedPackPayloadSource::External,
            });
            report_members.push(ManagedPackReportMember {
                id: source.id.clone(),
                indexed: None,
                exact_size: Some(source.size),
            });
            override_sources.push(ManagedPackOverrideSource {
                id: source.id,
                archive_index: source.archive_index,
                archive_path: source.archive_path,
                size: source.size,
            });
        }
        let projection = plan_managed_pack_transaction(session, payloads, self.removal_paths)?;
        let overrides_applied = override_sources.len();
        Ok(ManagedPackTransactionProjection {
            projection,
            overrides: override_sources,
            report: ManagedPackReportPlan {
                index: self.index,
                overrides_applied,
                members: report_members,
            },
        })
    }
}

pub struct ManagedPackTransactionProjection {
    projection: ManagedContentOperationProjection,
    overrides: Vec<ManagedPackOverrideSource>,
    report: ManagedPackReportPlan,
}

impl ManagedPackTransactionProjection {
    pub fn effect_paths(&self) -> Vec<PortableRelativePath> {
        self.projection.effect_paths()
    }

    pub fn seal(
        self,
        session: &ManagedContentTransactionSession,
    ) -> ContentResult<ManagedPackExecutionPlan> {
        Ok(ManagedPackExecutionPlan {
            transaction: self.projection.seal(session)?,
            overrides: self.overrides,
            report: self.report,
        })
    }
}

pub struct ManagedPackExecutionPlan {
    transaction: ManagedContentExecutionPlan,
    overrides: Vec<ManagedPackOverrideSource>,
    report: ManagedPackReportPlan,
}

impl ManagedPackExecutionPlan {
    pub fn into_parts(
        self,
    ) -> (
        ManagedContentExecutionPlan,
        Vec<ManagedPackOverrideSource>,
        ManagedPackReportPlan,
    ) {
        (self.transaction, self.overrides, self.report)
    }
}

pub fn inspect_managed_pack_plan<R>(
    archive: &mut R,
    options: PackInstallOptions<'_>,
) -> ContentResult<ManagedPackPlan>
where
    R: Read + Seek,
{
    let index = read_pack_index(archive)?;
    let selected = options
        .selected_paths
        .iter()
        .map(String::as_str)
        .collect::<HashSet<_>>();
    if !selected.is_empty() && options.include_overrides {
        return Err(ContentError::Invalid(
            "modpack overrides cannot be applied with selected files".to_string(),
        ));
    }
    let files = index
        .files
        .iter()
        .filter(|file| selected.is_empty() || selected.contains(file.path.as_str()))
        .cloned()
        .collect::<Vec<_>>();
    if !selected.is_empty() && files.len() != selected.len() {
        return Err(ContentError::ProviderMetadataInvalid(
            "the selected modpack files changed; review the pack again".to_string(),
        ));
    }
    let mut destinations = HashSet::with_capacity(files.len());
    let mut indexed = Vec::with_capacity(files.len());
    for (index, file) in files.into_iter().enumerate() {
        let path = normalize_relative_path(&file.path)?;
        destinations.insert(pack_destination_key(&path));
        indexed.push((
            ManagedContentPayloadId::new(&format!("pack-index-{index}"))
                .map_err(|_| invalid_pack_transfer_metadata())?,
            file.clone(),
            indexed_pack_transfer_contract(&file)?,
        ));
    }
    let overrides = if options.include_overrides {
        inspect_pack_overrides(archive)?
    } else {
        Vec::new()
    };
    if overrides
        .iter()
        .any(|source| destinations.contains(&pack_destination_key(&source.path)))
    {
        return Err(ContentError::ProviderMetadataInvalid(
            "modpack override replaces an indexed content file".to_string(),
        ));
    }
    for source in &overrides {
        if !destinations.insert(pack_destination_key(&source.path)) {
            return Err(ContentError::ProviderMetadataInvalid(
                "modpack contains duplicate file destinations".to_string(),
            ));
        }
    }
    let mut removal_paths = Vec::with_capacity(options.additional_guarded_paths.len());
    for path in options.additional_guarded_paths {
        let path = normalize_relative_path(path)?;
        if destinations.contains(&pack_destination_key(&path)) {
            return Err(ContentError::Invalid(
                "a stale managed path overlaps a modpack destination".to_string(),
            ));
        }
        removal_paths.push(path);
    }
    Ok(ManagedPackPlan {
        index,
        indexed,
        overrides,
        removal_paths,
    })
}

pub fn copy_managed_pack_override<R>(
    archive: &mut R,
    source: &ManagedPackOverrideSource,
    issued: ManagedContentIssuedTransfer,
    cancellation: TransferCancellation,
) -> Result<ManagedContentTransferSettlement, (ContentError, ManagedContentIssuedTransfer)>
where
    R: Read + Seek + Send,
{
    let mut zip = match zip::ZipArchive::new(archive) {
        Ok(zip) => zip,
        Err(error) => {
            return Err((
                ContentError::ProviderMetadataInvalid(format!("not a readable modpack: {error}")),
                issued,
            ));
        }
    };
    let entry = match zip.by_index(source.archive_index) {
        Ok(entry) => entry,
        Err(_) => {
            return Err((
                ContentError::ProviderMetadataInvalid(
                    "modpack override source changed before transfer".to_string(),
                ),
                issued,
            ));
        }
    };
    if entry.name() != source.archive_path || entry.size() != source.size {
        return Err((
            ContentError::ProviderMetadataInvalid(
                "modpack override source changed before transfer".to_string(),
            ),
            issued,
        ));
    }
    issued.copy_external(entry, cancellation).map_err(|issued| {
        (
            ContentError::Invalid("modpack transfer source kind changed".to_string()),
            issued,
        )
    })
}

fn inspect_pack_overrides<R>(archive: &mut R) -> ContentResult<Vec<ManagedPackOverride>>
where
    R: Read + Seek,
{
    archive.seek(SeekFrom::Start(0))?;
    let mut zip = zip::ZipArchive::new(archive).map_err(|error| {
        ContentError::ProviderMetadataInvalid(format!("not a readable modpack: {error}"))
    })?;
    let mut overrides = Vec::<ManagedPackOverride>::new();
    let mut positions = HashMap::<PackDestinationKey, (usize, &'static str)>::new();
    let mut extracted_files = 0_usize;
    let mut extracted_bytes = 0_u64;
    for root in [OVERRIDES, CLIENT_OVERRIDES] {
        let prefix = format!("{root}/");
        for archive_index in 0..zip.len() {
            let mut entry = zip.by_index(archive_index).map_err(|error| {
                ContentError::ProviderMetadataInvalid(format!("unreadable modpack: {error}"))
            })?;
            if entry.is_dir() {
                continue;
            }
            let Some(name) = entry.enclosed_name() else {
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
            if extracted_files >= MAX_OVERRIDE_FILES {
                return Err(ContentError::ProviderMetadataInvalid(
                    "modpack contains too many override files".to_string(),
                ));
            }
            let path = normalize_relative_path(&relative)?;
            let key = pack_destination_key(&path);
            let position = match positions.get(&key).copied() {
                None => {
                    let position = overrides.len();
                    positions.insert(key, (position, root));
                    position
                }
                Some((position, previous)) if previous == OVERRIDES && root == CLIENT_OVERRIDES => {
                    positions.insert(key, (position, root));
                    position
                }
                Some(_) => {
                    return Err(ContentError::ProviderMetadataInvalid(
                        "modpack contains a duplicate override path".to_string(),
                    ));
                }
            };
            let declared_size = entry.size();
            if declared_size > MAX_OVERRIDE_ENTRY_BYTES
                || extracted_bytes.saturating_add(declared_size) > MAX_OVERRIDE_TOTAL_BYTES
            {
                return Err(ContentError::ProviderMetadataInvalid(
                    "modpack overrides exceed the extraction limit".to_string(),
                ));
            }
            let (size, sha512) = digest_pack_archive_entry(
                &mut entry,
                MAX_OVERRIDE_ENTRY_BYTES.min(MAX_OVERRIDE_TOTAL_BYTES - extracted_bytes),
            )?;
            extracted_files += 1;
            extracted_bytes = extracted_bytes.saturating_add(size);
            let source = ManagedPackOverride {
                id: ManagedContentPayloadId::new(&format!("pack-override-{position}"))
                    .map_err(|_| invalid_pack_transfer_metadata())?,
                archive_index,
                archive_path: entry.name().to_string(),
                path,
                size,
                sha512,
            };
            if position == overrides.len() {
                overrides.push(source);
            } else {
                overrides[position] = source;
            }
        }
    }
    Ok(overrides)
}

fn digest_pack_archive_entry<R>(source: &mut R, limit: u64) -> ContentResult<(u64, [u8; 64])>
where
    R: Read,
{
    let mut copied = 0_u64;
    let mut digest = Sha512::new();
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
            return Ok((copied, digest.finalize().into()));
        }
        copied = copied.saturating_add(read as u64);
        if copied > limit {
            return Err(ContentError::ProviderMetadataInvalid(
                "modpack overrides exceed the extraction limit".to_string(),
            ));
        }
        digest.update(&buffer[..read]);
    }
}

fn indexed_pack_transfer_contract(file: &PackFile) -> ContentResult<TransferContract> {
    let digests = ExpectedTransferDigests::from_hex(file.sha1.as_deref(), file.sha512.as_deref())
        .map_err(|_| invalid_pack_transfer_metadata())?;
    match file.size {
        Some(size) if size <= MAX_MANAGED_TRANSFER_BYTES => exact_or_empty_contract(size, digests),
        Some(_) => Err(invalid_pack_transfer_metadata()),
        None => TransferContract::authenticated_below(
            NonZeroU64::new(MAX_MANAGED_TRANSFER_BYTES).expect("pack source limit is nonzero"),
            digests,
        )
        .map_err(|_| invalid_pack_transfer_metadata()),
    }
}

fn exact_or_empty_contract(
    size: u64,
    digests: ExpectedTransferDigests,
) -> ContentResult<TransferContract> {
    match NonZeroU64::new(size) {
        Some(size) => TransferContract::authenticated_exact(size, digests),
        None => TransferContract::authenticated_below(
            NonZeroU64::new(1).expect("one is nonzero"),
            digests,
        ),
    }
    .map_err(|_| invalid_pack_transfer_metadata())
}

fn invalid_pack_transfer_metadata() -> ContentError {
    ContentError::ProviderMetadataInvalid("modpack transfer metadata is invalid".to_string())
}

/// Read a pack's index without installing anything, so a caller can learn the
/// loader and Minecraft version it needs before creating an instance for it.
pub fn read_pack_index<R>(archive: &mut R) -> ContentResult<PackIndex>
where
    R: Read + Seek,
{
    archive.seek(SeekFrom::Start(0))?;
    let mut zip = zip::ZipArchive::new(archive).map_err(|error| {
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

/// Resolve `relative` under `root`, refusing anything that would escape it.
#[cfg(test)]
fn contained_path(root: &Path, relative: &str) -> ContentResult<PathBuf> {
    let relative = normalize_relative_path(relative)?;
    Ok(relative.join_under(root))
}

fn normalize_relative_path(relative: &str) -> ContentResult<PortableRelativePath> {
    let portable = PortableRelativePath::new_exact(relative).map_err(|_| {
        ContentError::ProviderMetadataInvalid(
            "modpack file uses an invalid portable path".to_string(),
        )
    })?;
    let managed_parent =
        managed_content_parent(portable_parent(&portable).as_ref()).map_err(|_| {
            ContentError::ProviderMetadataInvalid(
                "modpack file uses a launcher-reserved or non-canonical path".to_string(),
            )
        })?;
    if managed_parent.is_some()
        && ManagedContentFileName::new_exact(portable.file_name().as_str()).is_err()
    {
        return Err(ContentError::ProviderMetadataInvalid(
            "modpack file uses a launcher-reserved or non-canonical path".to_string(),
        ));
    }
    let reserved_name = managed_content_name_is_reserved(&portable.file_name());
    if reserved_name && (!portable.as_str().contains('/') || managed_parent.is_some()) {
        return Err(ContentError::ProviderMetadataInvalid(
            "modpack file uses a launcher-reserved or non-canonical path".to_string(),
        ));
    }
    Ok(portable)
}

fn pack_destination_key(path: &PortableRelativePath) -> PackDestinationKey {
    let parent_path = portable_parent(path);
    let managed_parent = managed_content_parent(parent_path.as_ref())
        .expect("normalized pack paths use exact managed parent spelling")
        .is_some();
    let parent = parent_path.as_ref().map(PortableRelativePath::key);
    let name = path.file_name();
    let name = if managed_parent {
        managed_content_name_key(&name)
    } else {
        name.key()
    };
    PackDestinationKey { parent, name }
}

fn portable_parent(path: &PortableRelativePath) -> Option<PortableRelativePath> {
    path.as_str().rsplit_once('/').map(|(parent, _)| {
        PortableRelativePath::new_exact(parent)
            .expect("an admitted portable path has an exact portable parent")
    })
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
    let unique_paths = files
        .iter()
        .map(|file| normalize_relative_path(&file.path).map(|path| pack_destination_key(&path)))
        .collect::<ContentResult<HashSet<_>>>()?;
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
    let sha1 = validate_pack_hash(file.hashes.sha1, 40, "sha1", path.as_str())?;
    let sha512 = validate_pack_hash(file.hashes.sha512, 128, "sha512", path.as_str())?;
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
        path: path.as_str().to_string(),
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
    fn nested_pack_paths_never_become_direct_managed_ownership() {
        let nested = PackFile {
            path: "mods/nested/example.jar".to_string(),
            url: "https://example.invalid/example.jar".to_string(),
            sha1: None,
            sha512: Some("a".repeat(128)),
            size: Some(1),
        };

        assert_eq!(nested.kind(), None);
    }

    #[test]
    fn pack_paths_must_use_canonical_portable_spelling() {
        let raw = r#"{
            "formatVersion": 1,
            "dependencies": { "minecraft": "1.21.6" },
            "files": [{
                "path": "mods/./example.jar",
                "hashes": { "sha1": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa" },
                "downloads": ["https://cdn.modrinth.com/example.jar"]
            }]
        }"#;
        assert!(parse_pack_index(raw).is_err());

        let parent_alias = r#"{
            "formatVersion": 1,
            "dependencies": { "minecraft": "1.21.6" },
            "files": [{
                "path": "Mods/example.jar",
                "hashes": { "sha1": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa" },
                "downloads": ["https://cdn.modrinth.com/example.jar"]
            }]
        }"#;
        assert!(parse_pack_index(parent_alias).is_err());

        let disabled_managed_leaf = r#"{
            "formatVersion": 1,
            "dependencies": { "minecraft": "1.21.6" },
            "files": [{
                "path": "mods/example.jar.disabled",
                "hashes": { "sha1": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa" },
                "downloads": ["https://cdn.modrinth.com/example.jar"]
            }]
        }"#;
        assert!(parse_pack_index(disabled_managed_leaf).is_err());
        assert_eq!(
            PackFile {
                path: "mods/example.jar.disabled".to_string(),
                url: "https://example.invalid/example.jar".to_string(),
                sha1: Some("a".repeat(40)),
                sha512: None,
                size: Some(1),
            }
            .kind(),
            None
        );

        let duplicate = r#"{
            "formatVersion": 1,
            "dependencies": { "minecraft": "1.21.6" },
            "files": [
                { "path": "mods/Straße.jar", "hashes": { "sha1": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa" }, "downloads": ["https://cdn.modrinth.com/a.jar"] },
                { "path": "MODS/STRASSE.JAR.disabled", "hashes": { "sha1": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb" }, "downloads": ["https://cdn.modrinth.com/b.jar"] }
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
        let mut archive_file = fs::File::open(&archive).expect("open pack archive");

        let error =
            read_pack_index(&mut archive_file).expect_err("oversized index must be rejected");
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
        assert!(contained_path(root, "./config/sodium.json").is_err());
    }

    #[test]
    fn launcher_manifest_paths_are_reserved_at_owned_roots() {
        let root = Path::new("/instances/aurora");

        for reserved in [
            "axial.content.json",
            "./axial.content.json",
            "AXIAL.CONTENT.JSON",
            "axial.content.json.DISABLED.disabled",
            ".axial-publication",
            ".axial-content-stage",
            ".axial-pack-import",
            ".axial-replacement-file",
            "mods/.axial-pack-import.jar",
            "resourcepacks/.axial-content-stage.zip.disabled",
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
        assert_eq!(
            contained_path(root, "config/.axial-pack-user.json")
                .expect("nested internal-looking path is user-owned"),
            root.join("config").join(".axial-pack-user.json")
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
        let archive = override_archive(
            "entry-limit",
            &[(
                "overrides/config/oversized.bin",
                vec![b'x'; MAX_OVERRIDE_ENTRY_BYTES as usize + 1],
            )],
        );
        let mut archive_file = fs::File::open(&archive).expect("open pack archive");

        assert!(inspect_pack_overrides(&mut archive_file).is_err());

        let _ = fs::remove_file(archive);
    }

    #[test]
    fn overrides_cannot_claim_launcher_manifest_paths() {
        let archive = override_archive(
            "manifest-path",
            &[("overrides/./axial.content.json", b"payload".to_vec())],
        );
        let mut archive_file = fs::File::open(&archive).expect("open pack archive");

        let error = inspect_pack_overrides(&mut archive_file)
            .expect_err("override must not claim launcher manifest paths");
        assert!(error.to_string().contains("invalid portable path"));

        let _ = fs::remove_file(archive);
    }

    #[test]
    fn cumulative_override_size_is_bounded() {
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
        let mut archive_file = fs::File::open(&archive).expect("open pack archive");

        assert!(inspect_pack_overrides(&mut archive_file).is_err());

        let _ = fs::remove_file(archive);
    }

    #[test]
    fn client_override_replacements_are_reported_once() {
        let archive = override_archive(
            "client-replacement",
            &[
                ("overrides/config/shared.bin", vec![b'a'; 128]),
                ("overrides/config/other.bin", vec![b'b'; 128]),
                ("client-overrides/config/shared.bin", vec![b'c'; 128]),
            ],
        );
        let mut archive_file = fs::File::open(&archive).expect("open pack archive");

        let planned = inspect_pack_overrides(&mut archive_file).expect("inspect overrides");
        assert_eq!(planned.len(), 2);
        assert_eq!(planned[0].path.as_str(), "config/shared.bin");
        assert_eq!(
            planned[0].archive_path,
            "client-overrides/config/shared.bin"
        );
        assert_eq!(planned[1].path.as_str(), "config/other.bin");
        assert_eq!(
            planned[0].sha512,
            Sha512::digest(vec![b'c'; 128]).as_slice()
        );

        let _ = fs::remove_file(archive);
    }

    #[test]
    fn managed_pack_inspection_binds_index_and_override_sources_without_writes() {
        let index = format!(
            r#"{{
                "formatVersion": 1,
                "game": "minecraft",
                "versionId": "1.0.0",
                "name": "Planned Pack",
                "dependencies": {{ "minecraft": "1.21.6" }},
                "files": [{{
                    "path": "mods/example.jar",
                    "hashes": {{ "sha512": "{}" }},
                    "downloads": ["https://cdn.modrinth.com/example.jar"],
                    "fileSize": 7
                }}]
            }}"#,
            "a".repeat(128)
        );
        let archive = override_archive(
            "managed-plan",
            &[
                (INDEX_FILE, index.into_bytes()),
                ("overrides/config/options.txt", b"server".to_vec()),
                ("client-overrides/config/options.txt", b"client".to_vec()),
            ],
        );
        let mut archive_file = fs::File::open(&archive).expect("open pack archive");

        let plan = inspect_managed_pack_plan(
            &mut archive_file,
            PackInstallOptions {
                selected_paths: &[],
                additional_guarded_paths: &["mods/stale.jar".to_string()],
                include_overrides: true,
            },
        )
        .expect("inspect managed pack");

        assert_eq!(plan.indexed.len(), 1);
        assert_eq!(plan.indexed[0].1.path, "mods/example.jar");
        assert_eq!(plan.overrides.len(), 1);
        assert_eq!(plan.overrides[0].path.as_str(), "config/options.txt");
        assert_eq!(
            plan.overrides[0].archive_path,
            "client-overrides/config/options.txt"
        );
        assert_eq!(plan.removal_paths[0].as_str(), "mods/stale.jar");

        let _ = fs::remove_file(archive);
    }

    #[test]
    fn managed_pack_inspection_rejects_override_index_collisions() {
        let index = format!(
            r#"{{
                "formatVersion": 1,
                "dependencies": {{ "minecraft": "1.21.6" }},
                "files": [{{
                    "path": "config/options.txt",
                    "hashes": {{ "sha512": "{}" }},
                    "downloads": ["https://cdn.modrinth.com/options.txt"]
                }}]
            }}"#,
            "b".repeat(128)
        );
        let archive = override_archive(
            "managed-collision",
            &[
                (INDEX_FILE, index.into_bytes()),
                ("overrides/config/options.txt", b"override".to_vec()),
            ],
        );
        let mut archive_file = fs::File::open(&archive).expect("open pack archive");

        assert!(
            inspect_managed_pack_plan(
                &mut archive_file,
                PackInstallOptions {
                    selected_paths: &[],
                    additional_guarded_paths: &[],
                    include_overrides: true,
                },
            )
            .is_err()
        );

        let _ = fs::remove_file(archive);
    }

    #[test]
    fn client_override_replacements_count_toward_the_extraction_limit() {
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
        let mut archive_file = fs::File::open(&archive).expect("open pack archive");

        let error = inspect_pack_overrides(&mut archive_file)
            .expect_err("replacement extraction must remain cumulatively bounded");
        assert!(error.to_string().contains("extraction limit"));

        let _ = fs::remove_file(archive);
    }

    #[test]
    fn duplicate_override_paths_are_rejected() {
        let archive = override_archive(
            "duplicate-path",
            &[
                ("overrides/config/Stra\u{df}e.bin", b"first".to_vec()),
                ("overrides/CONFIG/STRASSE.BIN", b"second".to_vec()),
            ],
        );
        let mut archive_file = fs::File::open(&archive).expect("open pack archive");

        let error =
            inspect_pack_overrides(&mut archive_file).expect_err("duplicate path must be rejected");
        assert!(matches!(&error, ContentError::ProviderMetadataInvalid(_)));
        assert!(error.to_string().contains("duplicate override path"));

        let _ = fs::remove_file(archive);
    }

    #[test]
    fn override_paths_reject_dot_components() {
        let archive = override_archive(
            "dot-path",
            &[("overrides/mods/./example.jar", b"override".to_vec())],
        );
        let mut archive_file = fs::File::open(&archive).expect("open pack archive");

        let error =
            inspect_pack_overrides(&mut archive_file).expect_err("dot component must be rejected");
        assert!(matches!(&error, ContentError::ProviderMetadataInvalid(_)));
        assert!(error.to_string().contains("invalid portable path"));

        let _ = fs::remove_file(archive);
    }

    #[test]
    fn managed_pack_availability_accepts_only_exact_regular_variants() {
        let root = std::env::temp_dir().join("axial-pack-managed-availability");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("mods")).expect("mods");
        let file = PackFile {
            path: "mods/example.jar".to_string(),
            url: "https://example.invalid/example.jar".to_string(),
            sha1: Some("a".repeat(40)),
            sha512: None,
            size: Some(1),
        };

        let empty = ManagedPackAvailability::capture(&root, std::slice::from_ref(&file))
            .expect("empty availability");
        assert!(!empty.contains(&file));

        fs::write(root.join("mods/example.jar.disabled"), b"disabled").expect("disabled");
        let disabled = ManagedPackAvailability::capture(&root, std::slice::from_ref(&file))
            .expect("disabled availability");
        assert!(disabled.contains(&file));
        fs::remove_file(root.join("mods/example.jar.disabled")).expect("remove disabled");

        fs::create_dir(root.join("mods/example.jar")).expect("directory alias");
        assert!(ManagedPackAvailability::capture(&root, std::slice::from_ref(&file)).is_err());
        fs::remove_dir(root.join("mods/example.jar")).expect("remove directory");

        fs::write(root.join("mods/EXAMPLE.JAR"), b"alias").expect("portable alias");
        assert!(ManagedPackAvailability::capture(&root, std::slice::from_ref(&file)).is_err());

        let _ = fs::remove_dir_all(root);
    }
}
