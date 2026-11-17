//! Eclipse Temurin 8 JRE as the managed `jre-legacy` source.
//!
//! Mojang's legacy runtime predates the Java 8 update the launcher requires
//! (8u312), so every Java 8 game and loader processor would be provisioned a
//! runtime that admission then refuses. This source takes the latest Temurin 8
//! JRE from Adoptium's metadata, admits the archive only when its size and
//! SHA-256 match that metadata, and expands it in memory within the managed
//! runtime tree bounds. The result is an ordinary component manifest: files
//! are published, verified and later reverified exactly like Mojang's.
//!
//! Process scratch is reserved before each fetch and before every expansion
//! allocation, and the digest and expansion run under a physical work
//! admission. The expanded bytes keep their reservation for the lifetime of
//! the archive that holds them.

use super::manifest::{
    ComponentManifest, ComponentManifestDownload, ComponentManifestDownloads,
    ComponentManifestFile, RuntimeDownloadManifest, RuntimeSourceReceipt,
};
use super::model::{JavaRuntimeLookupError, RuntimeId, RuntimeSourceFailureKind};
use crate::http::{self, FetchError};
use crate::portable_path::PortableRelativePath;
use serde::Deserialize;
use sha1::Digest as _;
use axial_resource::{
    PhysicalIoClass, PhysicalScratchPermit, PhysicalWorkOwner, PhysicalWorkRequest,
    process_physical_work,
};
use std::collections::{BTreeMap, HashMap};
use std::io::Read;
use std::sync::Arc;

pub(crate) const COMPONENT: &str = "jre-legacy";
const API: &str = "https://api.adoptium.net/v3/assets/latest/8/hotspot";
const ARCHIVE_PREFIX: &str = "https://github.com/adoptium/temurin8-binaries/releases/download/";
const MAX_METADATA_BYTES: u64 = 512 << 10;
const MAX_ARCHIVE_BYTES: u64 = 128 << 20;
const MAX_EXPANDED_BYTES: u64 = 512 << 20;
const MAX_ENTRIES: usize = 4096;
/// Archive headers scanned, including the top-level directory and PAX records.
const MAX_ARCHIVE_ENTRIES: usize = 2 * MAX_ENTRIES;
/// Scratch is grown in chunks of this size, or exactly what remains.
const CAPACITY_CHUNK_BYTES: u64 = 8 << 20;
/// Retained bookkeeping per manifest entry beyond its path and download URL
/// copies: the manifest record, its digest text and its encoded proof line.
const ENTRY_OVERHEAD_BYTES: u64 = 512;

/// File bytes keyed by manifest path, already proven by the archive digest,
/// with the process scratch reserved for them while any holder retains them.
pub(crate) struct RuntimeArchive {
    files: HashMap<String, Arc<[u8]>>,
    _capacity: Vec<PhysicalScratchPermit>,
}

impl RuntimeArchive {
    pub(crate) fn get(&self, path: &str) -> Option<&Arc<[u8]>> {
        self.files.get(path)
    }
}

impl PartialEq for RuntimeArchive {
    fn eq(&self, other: &Self) -> bool {
        self.files == other.files
    }
}

impl Eq for RuntimeArchive {}

impl std::fmt::Debug for RuntimeArchive {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RuntimeArchive")
            .field("files", &self.files.len())
            .finish_non_exhaustive()
    }
}

#[derive(Deserialize)]
struct Release {
    binary: Binary,
    version: ReleaseVersion,
}

/// Adoptium's version of the release; `security` is Java 8's update.
#[derive(Deserialize)]
struct ReleaseVersion {
    major: u32,
    security: u32,
}

#[derive(Deserialize)]
struct Binary {
    package: Package,
}

#[derive(Deserialize)]
struct Package {
    link: String,
    checksum: String,
    size: u64,
    name: String,
}

#[derive(Clone, Copy)]
enum Format {
    Zip,
    TarGz,
}

/// Adoptium's `(os, architecture)` for this host. ARM macOS and Windows run
/// the x64 build under translation, as Mojang's legacy runtime does.
fn host_target() -> Option<(&'static str, &'static str, Format)> {
    let (os, format) = match std::env::consts::OS {
        "windows" => ("windows", Format::Zip),
        "macos" => ("mac", Format::TarGz),
        "linux" => ("linux", Format::TarGz),
        _ => return None,
    };
    let arch = match (os, std::env::consts::ARCH) {
        (_, "x86_64") => "x64",
        ("windows", "x86") => "x86",
        ("windows" | "mac", "aarch64") => "x64",
        ("linux", "aarch64") => "aarch64",
        _ => return None,
    };
    Some((os, arch, format))
}

fn failure(kind: RuntimeSourceFailureKind, detail: impl Into<String>) -> JavaRuntimeLookupError {
    super::install::runtime_source_failure(&RuntimeId::from(COMPONENT), kind, detail.into())
}

fn fetch_failure(error: &FetchError, what: &str) -> JavaRuntimeLookupError {
    let kind = match error {
        FetchError::Refused | FetchError::TooLarge { .. } | FetchError::Allocation => {
            RuntimeSourceFailureKind::PolicyRejected
        }
        FetchError::SizeMismatch { .. } | FetchError::DigestMismatch { .. } => {
            RuntimeSourceFailureKind::IntegrityMismatch
        }
        _ if error
            .status()
            .is_some_and(|status| !http::is_retryable_status(status)) =>
        {
            RuntimeSourceFailureKind::MetadataInvalid
        }
        _ => RuntimeSourceFailureKind::Unavailable,
    };
    failure(kind, format!("{what} could not be fetched"))
}

/// Acquires and authenticates the Temurin 8 JRE for this host.
pub(super) async fn acquire() -> Result<RuntimeSourceReceipt, JavaRuntimeLookupError> {
    let (os, arch, format) =
        host_target().ok_or_else(|| JavaRuntimeLookupError::UnsupportedPlatform {
            component: COMPONENT.into(),
            platform: format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH),
        })?;
    let url = format!("{API}?architecture={arch}&image_type=jre&os={os}&vendor=eclipse");
    let work = process_physical_work();
    let metadata_scratch = work
        .reserve_scratch(MAX_METADATA_BYTES)
        .await
        .map_err(|_| capacity_failure())?;
    let metadata = http::fetch(
        http::client(),
        &url,
        MAX_METADATA_BYTES,
        http::Expected::default(),
    )
    .await
    .map_err(|error| fetch_failure(&error, "Temurin 8 metadata"))?;
    let package = select_package(&metadata.bytes, format)?;
    drop((metadata, metadata_scratch));
    let archive_scratch = work
        .reserve_scratch(package.size)
        .await
        .map_err(|_| capacity_failure())?;
    let archive = http::fetch(
        http::client(),
        &package.link,
        package.size,
        http::Expected {
            size: Some(package.size),
            sha1: None,
        },
    )
    .await
    .map_err(|error| fetch_failure(&error, "Temurin 8 archive"))?;
    let admission = work
        .admit(PhysicalWorkRequest::foreground(PhysicalIoClass::Heavy, 0))
        .await
        .map_err(|_| capacity_failure())?;
    admission
        .run(move |_| {
            let _archive_scratch = archive_scratch;
            let digest = hex::encode(sha2::Sha256::digest(&archive.bytes));
            if !digest.eq_ignore_ascii_case(&package.checksum) {
                return Err(failure(
                    RuntimeSourceFailureKind::IntegrityMismatch,
                    "Temurin 8 archive did not match its published SHA-256",
                ));
            }
            let expansion = expand(
                &archive.bytes,
                format,
                LIMITS,
                Capacity::new(work),
                &package.link,
            )?;
            drop(archive);
            receipt(&package.link, expansion)
        })
        .await
        .map_err(|_| capacity_failure())?
}

fn capacity_failure() -> JavaRuntimeLookupError {
    failure(
        RuntimeSourceFailureKind::Unavailable,
        "runtime expansion capacity is unavailable",
    )
}

fn select_package(bytes: &[u8], format: Format) -> Result<Package, JavaRuntimeLookupError> {
    let mut releases = serde_json::from_slice::<Vec<Release>>(bytes).map_err(|_| {
        failure(
            RuntimeSourceFailureKind::MetadataInvalid,
            "Temurin 8 metadata is invalid",
        )
    })?;
    let extension = match format {
        Format::Zip => ".zip",
        Format::TarGz => ".tar.gz",
    };
    let package = match releases.as_mut_slice() {
        [release]
            if super::model::JavaRequirement::for_current_host(8)
                .admits_version(release.version.major, release.version.security)
                .is_err() =>
        {
            return Err(failure(
                RuntimeSourceFailureKind::PolicyRejected,
                "Temurin 8 metadata names a release below the required Java 8 update",
            ));
        }
        [release] => std::mem::replace(
            &mut release.binary.package,
            Package {
                link: String::new(),
                checksum: String::new(),
                size: 0,
                name: String::new(),
            },
        ),
        _ => {
            return Err(failure(
                RuntimeSourceFailureKind::MetadataInvalid,
                "Temurin 8 metadata must name exactly one release",
            ));
        }
    };
    let file_name = package.link.rsplit('/').next().unwrap_or_default();
    if !package.link.starts_with(ARCHIVE_PREFIX)
        || file_name != package.name
        || !package.name.ends_with(extension)
        || package.size == 0
        || package.size > MAX_ARCHIVE_BYTES
        || package.checksum.len() != 64
        || !package
            .checksum
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(failure(
            RuntimeSourceFailureKind::PolicyRejected,
            "Temurin 8 metadata names an unadmitted package",
        ));
    }
    Ok(package)
}

enum Entry {
    Directory,
    File { bytes: Arc<[u8]>, executable: bool },
    Link(String),
}

#[derive(Clone, Copy)]
struct Limits {
    expanded_bytes: u64,
    entries: usize,
    archive_entries: usize,
}

const LIMITS: Limits = Limits {
    expanded_bytes: MAX_EXPANDED_BYTES,
    entries: MAX_ENTRIES,
    archive_entries: MAX_ARCHIVE_ENTRIES,
};

/// Process scratch held for retained expansion state, grown before each
/// allocation it covers.
struct Capacity {
    work: PhysicalWorkOwner,
    reserved: u64,
    used: u64,
    permits: Vec<PhysicalScratchPermit>,
}

impl Capacity {
    fn new(work: PhysicalWorkOwner) -> Self {
        Self {
            work,
            reserved: 0,
            used: 0,
            permits: Vec::new(),
        }
    }

    /// Reserves `bytes` more retained state before it is allocated.
    fn charge(&mut self, bytes: u64) -> Result<(), JavaRuntimeLookupError> {
        let needed = self
            .used
            .checked_add(bytes)
            .ok_or_else(capacity_failure)?;
        if needed > self.reserved {
            let deficit = needed - self.reserved;
            let permit = self
                .work
                .try_reserve_scratch(deficit.max(CAPACITY_CHUNK_BYTES))
                .map(|permit| (permit, deficit.max(CAPACITY_CHUNK_BYTES)))
                .or_else(|_| {
                    self.work
                        .try_reserve_scratch(deficit)
                        .map(|permit| (permit, deficit))
                })
                .map_err(|_| capacity_failure())?;
            self.reserved += permit.1;
            self.permits.extend(permit.0);
        }
        self.used = needed;
        Ok(())
    }
}

fn policy(detail: &'static str) -> JavaRuntimeLookupError {
    failure(RuntimeSourceFailureKind::PolicyRejected, detail)
}

fn unreadable() -> JavaRuntimeLookupError {
    failure(
        RuntimeSourceFailureKind::MetadataInvalid,
        "Temurin 8 archive is unreadable",
    )
}

/// The expanded tree below the archive's single top-level directory, with
/// every parent directory, in the managed layout (macOS bundles live under
/// `jre.bundle`).
struct Expansion {
    limits: Limits,
    capacity: Capacity,
    prefix: Option<String>,
    headers: usize,
    /// Declared bytes of every scanned entry, kept or skipped.
    scanned: u64,
    /// Length of the download URL each file's manifest record repeats.
    link: usize,
    entries: BTreeMap<String, Entry>,
    keys: HashMap<crate::portable_path::PortablePathKey, bool>,
}

impl Expansion {
    /// Admits one archive header before any of its data is read.
    fn scan(&mut self, archive_path: &str, size: u64) -> Result<Option<String>, JavaRuntimeLookupError> {
        self.headers += 1;
        if self.headers > self.limits.archive_entries {
            return Err(policy("Temurin 8 archive has too many entries"));
        }
        self.scanned = self
            .scanned
            .checked_add(size)
            .filter(|scanned| *scanned <= self.limits.expanded_bytes)
            .ok_or_else(|| policy("Temurin 8 archive is too large"))?;
        let path = archive_path.trim_end_matches('/');
        let (top, rest) = path.split_once('/').unwrap_or((path, ""));
        match &self.prefix {
            Some(prefix) if prefix != top => {
                return Err(policy("Temurin 8 archive has more than one top-level directory"));
            }
            Some(_) => {}
            None => self.prefix = Some(top.to_owned()),
        }
        if rest.is_empty() {
            return Ok(None);
        }
        Ok(Some(if cfg!(target_os = "macos") {
            format!("jre.bundle/{rest}")
        } else {
            rest.to_owned()
        }))
    }

    /// Charges and records one entry, its generated parents included, refusing
    /// portable-key collisions and paths below a file or link.
    fn insert(&mut self, path: String, entry: Entry) -> Result<(), JavaRuntimeLookupError> {
        let directory = matches!(entry, Entry::Directory);
        let mut ancestors = Vec::new();
        let mut parent = path.as_str();
        while let Some((above, _)) = parent.rsplit_once('/') {
            ancestors.push(above);
            parent = above;
        }
        for ancestor in ancestors.into_iter().rev() {
            let key = PortableRelativePath::new_exact(ancestor)
                .map_err(|_| policy("Temurin 8 archive has an unsafe path"))?
                .key();
            match self.keys.get(&key) {
                Some(true) => {}
                Some(false) => {
                    return Err(policy("Temurin 8 archive has an entry below a file"));
                }
                None => self.record(ancestor.to_owned(), key, Entry::Directory)?,
            }
        }
        let key = PortableRelativePath::new_exact(&path)
            .map_err(|_| policy("Temurin 8 archive has an unsafe path"))?
            .key();
        match self.keys.get(&key) {
            Some(true) if directory => Ok(()),
            Some(_) => Err(policy("Temurin 8 archive has colliding entries")),
            None => self.record(path, key, entry),
        }
    }

    fn record(
        &mut self,
        path: String,
        key: crate::portable_path::PortablePathKey,
        entry: Entry,
    ) -> Result<(), JavaRuntimeLookupError> {
        if self.entries.len() >= self.limits.entries {
            return Err(policy("Temurin 8 archive has too many entries"));
        }
        let link = if matches!(entry, Entry::File { .. }) { self.link } else { 0 };
        let bytes = (path.len() as u64)
            .checked_mul(3)
            .and_then(|bytes| bytes.checked_add((link as u64).checked_mul(2)?))
            .and_then(|bytes| bytes.checked_add(ENTRY_OVERHEAD_BYTES))
            .ok_or_else(capacity_failure)?;
        self.capacity.charge(bytes)?;
        self.keys.insert(key, matches!(entry, Entry::Directory));
        self.entries.insert(path, entry);
        Ok(())
    }

    /// Reads a file's declared bytes into retained storage reserved first.
    fn read(&mut self, reader: impl Read, size: u64) -> Result<Arc<[u8]>, JavaRuntimeLookupError> {
        if self.entries.len() >= self.limits.entries {
            return Err(policy("Temurin 8 archive has too many entries"));
        }
        self.capacity.charge(size)?;
        let length = usize::try_from(size).map_err(|_| policy("Temurin 8 entry is too large"))?;
        let mut bytes: Arc<[u8]> = std::iter::repeat_n(0, length).collect();
        let mut reader = reader.take(size + 1);
        reader
            .read_exact(Arc::get_mut(&mut bytes).expect("fresh archive entry"))
            .map_err(|_| unreadable())?;
        if reader.read(&mut [0]).map_err(|_| unreadable())? != 0 {
            return Err(failure(
                RuntimeSourceFailureKind::MetadataInvalid,
                "Temurin 8 entry size changed",
            ));
        }
        Ok(bytes)
    }
}

fn expand(
    archive: &[u8],
    format: Format,
    limits: Limits,
    capacity: Capacity,
    link: &str,
) -> Result<Expansion, JavaRuntimeLookupError> {
    let mut expansion = Expansion {
        limits,
        capacity,
        prefix: None,
        headers: 0,
        scanned: 0,
        link: link.len(),
        entries: BTreeMap::new(),
        keys: HashMap::new(),
    };
    match format {
        Format::Zip => {
            let mut zip =
                zip::ZipArchive::new(std::io::Cursor::new(archive)).map_err(|_| unreadable())?;
            if zip.len() > limits.archive_entries {
                return Err(policy("Temurin 8 archive has too many entries"));
            }
            for index in 0..zip.len() {
                let file = zip.by_index(index).map_err(|_| unreadable())?;
                let name = file.name().map_err(|_| unreadable())?.replace('\\', "/");
                let size = if file.is_file() { file.size() } else { 0 };
                let Some(path) = expansion.scan(&name, size)? else {
                    if !file.is_dir() {
                        return Err(policy("Temurin 8 archive has a top-level file"));
                    }
                    continue;
                };
                let entry = if file.is_dir() {
                    Entry::Directory
                } else if file.is_file() {
                    let executable = file.unix_mode().is_some_and(|mode| mode & 0o111 != 0);
                    Entry::File {
                        bytes: expansion.read(file, size)?,
                        executable,
                    }
                } else {
                    return Err(unreadable());
                };
                expansion.insert(path, entry)?;
            }
        }
        Format::TarGz => {
            let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(archive));
            for entry in tar.entries().map_err(|_| unreadable())? {
                let entry = entry.map_err(|_| unreadable())?;
                let name = entry
                    .path()
                    .map_err(|_| unreadable())?
                    .to_str()
                    .ok_or_else(unreadable)?
                    .to_owned();
                let header = entry.header();
                let kind = header.entry_type();
                let size = header.size().map_err(|_| unreadable())?;
                let Some(path) = expansion.scan(&name, size)? else {
                    if !kind.is_dir() {
                        return Err(policy("Temurin 8 archive has a top-level file"));
                    }
                    continue;
                };
                let value = if kind.is_dir() {
                    Entry::Directory
                } else if kind.is_file() {
                    let executable = header.mode().is_ok_and(|mode| mode & 0o111 != 0);
                    Entry::File {
                        bytes: expansion.read(entry, size)?,
                        executable,
                    }
                } else if kind.is_symlink() {
                    let target = header
                        .link_name()
                        .map_err(|_| unreadable())?
                        .ok_or_else(unreadable)?;
                    Entry::Link(target.to_str().ok_or_else(unreadable)?.to_owned())
                } else if matches!(
                    kind,
                    tar::EntryType::XGlobalHeader | tar::EntryType::XHeader
                ) {
                    continue;
                } else {
                    return Err(unreadable());
                };
                expansion.insert(path, value)?;
            }
        }
    }
    Ok(expansion)
}

/// The component manifest for the expanded tree and the receipt that carries
/// its proven bytes and their reservation.
fn receipt(link: &str, expansion: Expansion) -> Result<RuntimeSourceReceipt, JavaRuntimeLookupError> {
    let mut files = HashMap::with_capacity(expansion.entries.len());
    let mut archive = HashMap::new();
    for (path, entry) in expansion.entries {
        let file = match entry {
            Entry::Directory => ComponentManifestFile {
                kind: "directory".into(),
                executable: false,
                downloads: None,
                target: None,
            },
            Entry::File { bytes, executable } => {
                let file = ComponentManifestFile {
                    kind: "file".into(),
                    executable,
                    downloads: Some(ComponentManifestDownloads {
                        raw: Some(ComponentManifestDownload {
                            url: link.to_owned(),
                            sha1: Some(hex::encode(sha1::Sha1::digest(&bytes))),
                            size: Some(bytes.len() as u64),
                        }),
                        lzma: None,
                    }),
                    target: None,
                };
                archive.insert(path.clone(), bytes);
                file
            }
            Entry::Link(target) => ComponentManifestFile {
                kind: "link".into(),
                executable: false,
                downloads: None,
                target: Some(target),
            },
        };
        files.insert(path, file);
    }
    if !files.contains_key(super::layout::runtime_java_relative_path()) {
        return Err(failure(
            RuntimeSourceFailureKind::MetadataInvalid,
            "Temurin 8 archive has no Java executable",
        ));
    }
    let manifest = ComponentManifest { files };
    let bytes = super::manifest::component_manifest_proof_bytes(&manifest).map_err(|_| {
        failure(
            RuntimeSourceFailureKind::MetadataInvalid,
            "manifest encoding failed",
        )
    })?;
    Ok(RuntimeSourceReceipt::from_archive(
        RuntimeId::from(COMPONENT),
        RuntimeDownloadManifest {
            url: link.to_owned(),
            sha1: hex::encode(sha1::Sha1::digest(&bytes)),
            size: bytes.len() as u64,
        },
        bytes,
        manifest,
        Arc::new(RuntimeArchive {
            files: archive,
            _capacity: expansion.capacity.permits,
        }),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tar_gz(entries: &[(&str, &[u8], u32)], link: Option<(&str, &str)>) -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        for (path, bytes, mode) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_size(bytes.len() as u64);
            header.set_mode(*mode);
            header.set_cksum();
            builder.append_data(&mut header, path, *bytes).unwrap();
        }
        if let Some((path, target)) = link {
            let mut header = tar::Header::new_gnu();
            header.set_entry_type(tar::EntryType::Symlink);
            header.set_size(0);
            builder.append_link(&mut header, path, target).unwrap();
        }
        let tar = builder.into_inner().unwrap();
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        std::io::Write::write_all(&mut gz, &tar).unwrap();
        gz.finish().unwrap()
    }

    #[tokio::test]
    async fn tree_verification_refuses_retained_source_capacity_without_waiting_for_itself() {
        use super::super::layout::ManagedRuntimeCache;
        use axial_resource::PhysicalWorkClass;
        use std::time::Duration;
        let work = PhysicalWorkOwner::isolated_for_test();
        let java = runtime_java_relative_path_in_archive();
        let archive = tar_gz(
            &[
                (&java, b"java", 0o755),
                ("jdk8u512-b09-jre/lib/jvm.cfg", b"jvm", 0o644),
            ],
            None,
        );
        let source = receipt(
            LINK,
            expand_with(&archive, Format::TarGz, LIMITS, &work).unwrap(),
        )
        .unwrap();
        let retained_before = available(&work);
        let cache = ManagedRuntimeCache::isolated_for_test().unwrap();
        let root = cache.component_root(COMPONENT).unwrap();
        std::fs::create_dir_all(&root).unwrap();
        for (relative, member) in &source.manifest().files {
            let path = root.join(relative);
            if member.kind == "directory" {
                std::fs::create_dir_all(path).unwrap();
            } else {
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::write(&path, &**source.archive().unwrap().get(relative).unwrap()).unwrap();
                #[cfg(unix)]
                if member.executable {
                    use std::os::unix::fs::PermissionsExt;
                    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
                }
            }
        }
        std::fs::write(
            root.join(super::super::manifest::COMPONENT_MANIFEST_PROOF_FILE),
            source.bytes(),
        )
        .unwrap();
        std::fs::write(root.join(".axial-ready"), b"ready").unwrap();
        let component = cache.admit_component(COMPONENT).unwrap().unwrap();
        let held = work
            .try_reserve_scratch(retained_before - (8 << 20))
            .unwrap();
        let (_sender, cancellation) = crate::cancellation::channel();
        let under_contention = super::super::install::runtime_tree_matches_source_inner(
            &component.root,
            &source,
            cancellation.thread(),
            work.clone(),
        )
        .await;
        let exhausted = work.try_reserve_scratch(8 << 20).unwrap();
        let refusal = tokio::time::timeout(
            Duration::from_secs(1),
            super::super::install::runtime_tree_matches_source_inner(
                &component.root,
                &source,
                cancellation.thread(),
                work.clone(),
            ),
        )
        .await;
        drop(exhausted);
        drop(held);
        let restored = work.snapshot(PhysicalWorkClass::Foreground);
        let control = super::super::install::runtime_tree_matches_source_inner(
            &component.root,
            &source,
            cancellation.thread(),
            work.clone(),
        )
        .await;
        let settled = work.snapshot(PhysicalWorkClass::Foreground);
        drop((component, source));
        let released = available(&work);
        assert!(
            retained_before < work.scratch_limit_bytes(),
            "real expansion retains scratch"
        );
        assert_eq!(restored.available_scratch_bytes, retained_before);
        assert_eq!(settled.available_scratch_bytes, retained_before);
        assert_eq!(released, work.scratch_limit_bytes());
        assert!(
            under_contention.expect("source-sized verification fits under retained-source contention"),
            "small real source verifies with 8 MiB available"
        );
        let failure = refusal
            .expect("retained source cannot wait for its own scratch")
            .unwrap_err();
        assert!(
            matches!(failure, JavaRuntimeLookupError::RuntimeSource(failure)
            if failure.kind() == RuntimeSourceFailureKind::Unavailable
                && failure.detail() == "runtime verification capacity is unavailable")
        );
        assert!(control.unwrap());
    }

    #[test]
    fn metadata_admits_only_one_digest_bound_temurin_package() {
        let package = |link: &str, checksum: &str, size: u64| {
            serde_json::json!([{ "binary": { "package": {
                "link": link, "checksum": checksum, "size": size,
                "name": link.rsplit('/').next().unwrap(),
            }}, "version": { "major": 8, "minor": 0, "security": 512 }}])
            .to_string()
        };
        let good =
            format!("{ARCHIVE_PREFIX}jdk8u512-b09/OpenJDK8U-jre_x64_linux_hotspot_8u512b09.tar.gz");
        let digest = "a".repeat(64);
        assert!(select_package(package(&good, &digest, 10).as_bytes(), Format::TarGz).is_ok());
        for (link, checksum, size, format) in [
            (
                "https://example.com/OpenJDK8U.tar.gz",
                digest.as_str(),
                10,
                Format::TarGz,
            ),
            (good.as_str(), "not-a-digest", 10, Format::TarGz),
            (good.as_str(), digest.as_str(), 0, Format::TarGz),
            (
                good.as_str(),
                digest.as_str(),
                MAX_ARCHIVE_BYTES + 1,
                Format::TarGz,
            ),
            (good.as_str(), digest.as_str(), 10, Format::Zip),
        ] {
            assert!(
                select_package(package(link, checksum, size).as_bytes(), format).is_err(),
                "{link}"
            );
        }
        assert!(select_package(b"[]", Format::TarGz).is_err());
        let release = |major: u32, security: u32| {
            let mut value: serde_json::Value =
                serde_json::from_str(&package(&good, &digest, 10)).unwrap();
            value[0]["version"] = serde_json::json!({ "major": major, "security": security });
            value.to_string()
        };
        assert!(select_package(release(8, 312).as_bytes(), Format::TarGz).is_ok());
        for (major, security) in [(8, 311), (11, 512), (0, 0)] {
            assert!(
                select_package(release(major, security).as_bytes(), Format::TarGz).is_err(),
                "{major}u{security}"
            );
        }
        let unversioned = serde_json::json!([{ "binary": { "package": {
            "link": good, "checksum": digest, "size": 10,
            "name": good.rsplit('/').next().unwrap(),
        }}}]);
        assert!(select_package(unversioned.to_string().as_bytes(), Format::TarGz).is_err());
    }

    const LINK: &str = "https://example.invalid/jre.tar.gz";

    fn expand_with(
        archive: &[u8],
        format: Format,
        limits: Limits,
        work: &PhysicalWorkOwner,
    ) -> Result<Expansion, RuntimeSourceFailureKind> {
        expand(archive, format, limits, Capacity::new(work.clone()), LINK).map_err(kind)
    }

    fn kind(error: JavaRuntimeLookupError) -> RuntimeSourceFailureKind {
        match error {
            JavaRuntimeLookupError::RuntimeSource(failure) => failure.kind(),
            other => panic!("{other:?}"),
        }
    }

    fn available(work: &PhysicalWorkOwner) -> u64 {
        work.snapshot(axial_resource::PhysicalWorkClass::Foreground)
            .available_scratch_bytes
    }

    fn zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        for (path, bytes) in entries {
            if path.ends_with('/') {
                writer
                    .add_directory(*path, zip::write::SimpleFileOptions::default())
                    .unwrap();
            } else {
                writer
                    .start_file(*path, zip::write::SimpleFileOptions::default())
                    .unwrap();
                std::io::Write::write_all(&mut writer, bytes).unwrap();
            }
        }
        writer.finish().unwrap().into_inner()
    }

    #[test]
    fn expansion_builds_a_complete_manifest_and_refuses_unsafe_paths() {
        let work = PhysicalWorkOwner::isolated_for_test();
        let java = runtime_java_relative_path_in_archive();
        let archive = tar_gz(
            &[
                (&java, b"java", 0o755),
                ("jdk8u512-b09-jre/lib/rt.jar", b"classes", 0o644),
            ],
            Some(("jdk8u512-b09-jre/lib/link", "rt.jar")),
        );
        let expansion = expand_with(&archive, Format::TarGz, LIMITS, &work).unwrap();
        let source = receipt(LINK, expansion).unwrap();
        let manifest = source.manifest();
        let java_path = super::super::layout::runtime_java_relative_path();
        assert!(manifest.files[java_path].executable);
        assert_eq!(
            manifest.files[java_path.rsplit_once('/').unwrap().0].kind,
            "directory"
        );
        assert_eq!(manifest.files[&prefixed("lib/link")].kind, "link");
        assert_eq!(
            &**source.archive().unwrap().get(&prefixed("lib/rt.jar")).unwrap(),
            b"classes"
        );
        // The receipt's archive holds its reservation until the last holder drops it.
        let archive = source.archive().unwrap().clone();
        drop(source);
        assert!(available(&work) < work.scratch_limit_bytes());
        drop(archive);
        assert_eq!(available(&work), work.scratch_limit_bytes());

        let unsafe_archive = tar_gz_raw_path("jdk/../../escape", b"x");
        assert!(expand_with(&unsafe_archive, Format::TarGz, LIMITS, &work).is_err());
        let missing = tar_gz(&[("jdk8u512-b09-jre/lib/rt.jar", b"classes", 0o644)], None);
        let missing = expand_with(&missing, Format::TarGz, LIMITS, &work).unwrap();
        assert!(receipt(LINK, missing).is_err());
    }

    #[test]
    fn expansion_refuses_layout_conflicts_through_real_decoders() {
        let work = PhysicalWorkOwner::isolated_for_test();
        for entries in [
            // Two archive prefixes, and a file beside the single prefix.
            &[("jre-a/lib/a", &b"a"[..]), ("jre-b/lib/b", b"b")][..],
            &[("jre/lib/a", b"a"), ("README", b"r")],
            // Portable keys collide, and an entry below a file.
            &[("jre/lib/Rt.jar", b"a"), ("jre/lib/rt.jar", b"b")],
            &[("jre/lib", b"a"), ("jre/lib/rt.jar", b"b")],
            &[("jre/lib/rt.jar", b"a"), ("jre/lib", b"b")],
        ] {
            let tar = tar_gz(&entries.iter().map(|(p, b)| (*p, *b, 0o644)).collect::<Vec<_>>(), None);
            assert_eq!(
                expand_with(&tar, Format::TarGz, LIMITS, &work).err(),
                Some(RuntimeSourceFailureKind::PolicyRejected),
                "{entries:?}"
            );
            assert_eq!(
                expand_with(&zip(entries), Format::Zip, LIMITS, &work).err(),
                Some(RuntimeSourceFailureKind::PolicyRejected),
                "{entries:?}"
            );
        }
        let explicit = zip(&[("jre/", b""), ("jre/lib/", b""), ("jre/lib/rt.jar", b"x")]);
        let expansion = expand_with(&explicit, Format::Zip, LIMITS, &work).unwrap();
        // `lib` and `lib/rt.jar`, below `jre.bundle` on macOS.
        assert_eq!(expansion.entries.len(), if cfg!(target_os = "macos") { 3 } else { 2 });
        drop(expansion);
        assert_eq!(available(&work), work.scratch_limit_bytes());
    }

    #[test]
    fn expansion_refuses_declared_growth_before_reading_it() {
        let work = PhysicalWorkOwner::isolated_for_test();
        let tight = Limits {
            expanded_bytes: 1024,
            entries: 3,
            archive_entries: 4,
        };
        let large = vec![7; 2048];
        // The declared size is refused before its data is decompressed.
        let tar = tar_gz(&[("jre/lib/large", &large, 0o644)], None);
        assert_eq!(
            expand_with(&tar, Format::TarGz, tight, &work).err(),
            Some(RuntimeSourceFailureKind::PolicyRejected)
        );
        assert_eq!(
            expand_with(&zip(&[("jre/lib/large", &large)]), Format::Zip, tight, &work).err(),
            Some(RuntimeSourceFailureKind::PolicyRejected)
        );
        // Generated parents count as entries; skipped headers count as scanned.
        let deep = tar_gz(&[("jre/a/b/c/d", b"x", 0o644)], None);
        assert_eq!(
            expand_with(&deep, Format::TarGz, tight, &work).err(),
            Some(RuntimeSourceFailureKind::PolicyRejected)
        );
        let headers: Vec<_> = ["jre/", "jre/a/", "jre/b/", "jre/c/", "jre/d/"]
            .into_iter()
            .map(|path| (path, &b""[..]))
            .collect();
        assert_eq!(
            expand_with(&zip(&headers), Format::Zip, tight, &work).err(),
            Some(RuntimeSourceFailureKind::PolicyRejected)
        );
        assert!(expand_with(&zip(&headers[..3]), Format::Zip, tight, &work).is_ok());

        // Retained bytes need process scratch reserved before allocation.
        let held = work
            .try_reserve_scratch(work.scratch_limit_bytes() - (1 << 20))
            .unwrap();
        let two_mib = vec![0; 2 << 20];
        let tar = tar_gz(&[("jre/lib/large", &two_mib, 0o644)], None);
        assert_eq!(
            expand_with(&tar, Format::TarGz, LIMITS, &work).err(),
            Some(RuntimeSourceFailureKind::Unavailable)
        );
        drop(held);
        assert!(expand_with(&tar, Format::TarGz, LIMITS, &work).is_ok());
        assert_eq!(available(&work), work.scratch_limit_bytes());
    }

    fn prefixed(path: &str) -> String {
        if cfg!(target_os = "macos") {
            format!("jre.bundle/{path}")
        } else {
            path.to_owned()
        }
    }

    fn runtime_java_relative_path_in_archive() -> String {
        let managed = super::super::layout::runtime_java_relative_path();
        let inner = managed.strip_prefix("jre.bundle/").unwrap_or(managed);
        format!("jdk8u512-b09-jre/{inner}")
    }

    fn tar_gz_raw_path(path: &str, bytes: &[u8]) -> Vec<u8> {
        let mut header = tar::Header::new_gnu();
        header.as_gnu_mut().unwrap().name[..path.len()].copy_from_slice(path.as_bytes());
        header.set_size(bytes.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        let mut builder = tar::Builder::new(Vec::new());
        builder.append(&header, bytes).unwrap();
        let tar = builder.into_inner().unwrap();
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        std::io::Write::write_all(&mut gz, &tar).unwrap();
        gz.finish().unwrap()
    }
}
