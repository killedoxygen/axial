//! Eclipse Temurin 8 JRE as the managed `jre-legacy` source.
//!
//! Mojang's legacy runtime predates the Java 8 update the launcher requires
//! (8u312), so every Java 8 game and loader processor would be provisioned a
//! runtime that admission then refuses. This source takes the latest Temurin 8
//! JRE from Adoptium's metadata, admits the archive only when its size and
//! SHA-256 match that metadata, and expands it in memory within the managed
//! runtime tree bounds. The result is an ordinary component manifest: files
//! are published, verified and later reverified exactly like Mojang's.

use super::manifest::{
    ComponentManifest, ComponentManifestDownload, ComponentManifestDownloads,
    ComponentManifestFile, RuntimeDownloadManifest, RuntimeSourceReceipt,
};
use super::model::{JavaRuntimeLookupError, RuntimeId, RuntimeSourceFailureKind};
use crate::http::{self, FetchError};
use crate::portable_path::PortableRelativePath;
use serde::Deserialize;
use sha1::Digest as _;
use std::collections::{BTreeSet, HashMap};
use std::io::Read;
use std::sync::Arc;

pub(crate) const COMPONENT: &str = "jre-legacy";
const API: &str = "https://api.adoptium.net/v3/assets/latest/8/hotspot";
const ARCHIVE_PREFIX: &str = "https://github.com/adoptium/temurin8-binaries/releases/download/";
const MAX_METADATA_BYTES: u64 = 512 << 10;
const MAX_ARCHIVE_BYTES: u64 = 128 << 20;
const MAX_EXPANDED_BYTES: u64 = 512 << 20;
const MAX_ENTRIES: usize = 4096;

/// File bytes keyed by manifest path, already proven by the archive digest.
pub(crate) type RuntimeArchive = HashMap<String, Arc<[u8]>>;

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
    let metadata = http::fetch(
        http::client(),
        &url,
        MAX_METADATA_BYTES,
        http::Expected::default(),
    )
    .await
    .map_err(|error| fetch_failure(&error, "Temurin 8 metadata"))?;
    let package = select_package(&metadata.bytes, format)?;
    let archive = http::fetch(
        http::client(),
        &package.link,
        MAX_ARCHIVE_BYTES,
        http::Expected {
            size: Some(package.size),
            sha1: None,
        },
    )
    .await
    .map_err(|error| fetch_failure(&error, "Temurin 8 archive"))?;
    let digest = hex::encode(sha2::Sha256::digest(&archive.bytes));
    if !digest.eq_ignore_ascii_case(&package.checksum) {
        return Err(failure(
            RuntimeSourceFailureKind::IntegrityMismatch,
            "Temurin 8 archive did not match its published SHA-256",
        ));
    }
    let link = package.link.clone();
    let files = tokio::task::spawn_blocking(move || expand(&archive.bytes, format))
        .await
        .map_err(|_| {
            failure(
                RuntimeSourceFailureKind::MetadataInvalid,
                "archive expansion stopped",
            )
        })??;
    receipt(&link, files)
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
    File { bytes: Vec<u8>, executable: bool },
    Link(String),
}

/// Maps an archive path below its single top-level directory to the managed
/// layout: macOS bundles live under `jre.bundle`.
fn managed_path(archive_path: &str) -> Option<String> {
    let path = archive_path.trim_end_matches('/');
    let (_top, rest) = path.split_once('/')?;
    if rest.is_empty() {
        return None;
    }
    Some(if cfg!(target_os = "macos") {
        format!("jre.bundle/{rest}")
    } else {
        rest.to_owned()
    })
}

struct Expansion {
    entries: HashMap<String, Entry>,
    bytes: u64,
}

impl Expansion {
    fn insert(&mut self, archive_path: &str, entry: Entry) -> Result<(), JavaRuntimeLookupError> {
        let Some(path) = managed_path(archive_path) else {
            return Ok(());
        };
        let invalid = || {
            failure(
                RuntimeSourceFailureKind::PolicyRejected,
                "Temurin 8 archive has an unsafe path",
            )
        };
        PortableRelativePath::new_exact(&path).map_err(|_| invalid())?;
        if let Entry::File { bytes, .. } = &entry {
            self.bytes = self
                .bytes
                .checked_add(bytes.len() as u64)
                .filter(|total| *total <= MAX_EXPANDED_BYTES)
                .ok_or_else(|| {
                    failure(
                        RuntimeSourceFailureKind::PolicyRejected,
                        "Temurin 8 archive is too large",
                    )
                })?;
        }
        if self.entries.len() >= MAX_ENTRIES || self.entries.insert(path, entry).is_some() {
            return Err(failure(
                RuntimeSourceFailureKind::PolicyRejected,
                "Temurin 8 archive has too many or duplicate entries",
            ));
        }
        Ok(())
    }
}

fn read_bounded(reader: impl Read, size: u64) -> Result<Vec<u8>, JavaRuntimeLookupError> {
    if size > MAX_EXPANDED_BYTES {
        return Err(failure(
            RuntimeSourceFailureKind::PolicyRejected,
            "Temurin 8 entry is too large",
        ));
    }
    let mut bytes = Vec::with_capacity(size as usize);
    reader.take(size + 1).read_to_end(&mut bytes).map_err(|_| {
        failure(
            RuntimeSourceFailureKind::MetadataInvalid,
            "Temurin 8 archive is unreadable",
        )
    })?;
    if bytes.len() as u64 != size {
        return Err(failure(
            RuntimeSourceFailureKind::MetadataInvalid,
            "Temurin 8 entry size changed",
        ));
    }
    Ok(bytes)
}

fn expand(
    archive: &[u8],
    format: Format,
) -> Result<HashMap<String, Entry>, JavaRuntimeLookupError> {
    let unreadable = || {
        failure(
            RuntimeSourceFailureKind::MetadataInvalid,
            "Temurin 8 archive is unreadable",
        )
    };
    let mut expansion = Expansion {
        entries: HashMap::new(),
        bytes: 0,
    };
    match format {
        Format::Zip => {
            let mut zip =
                zip::ZipArchive::new(std::io::Cursor::new(archive)).map_err(|_| unreadable())?;
            for index in 0..zip.len() {
                let file = zip.by_index(index).map_err(|_| unreadable())?;
                let name = file.name().map_err(|_| unreadable())?.replace('\\', "/");
                let entry = if file.is_dir() {
                    Entry::Directory
                } else if file.is_file() {
                    let executable = file.unix_mode().is_some_and(|mode| mode & 0o111 != 0);
                    let size = file.size();
                    Entry::File {
                        bytes: read_bounded(file, size)?,
                        executable,
                    }
                } else {
                    return Err(unreadable());
                };
                expansion.insert(&name, entry)?;
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
                let value = if kind.is_dir() {
                    Entry::Directory
                } else if kind.is_file() {
                    let executable = header.mode().is_ok_and(|mode| mode & 0o111 != 0);
                    let size = header.size().map_err(|_| unreadable())?;
                    Entry::File {
                        bytes: read_bounded(entry, size)?,
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
                expansion.insert(&name, value)?;
            }
        }
    }
    Ok(expansion.entries)
}

/// The component manifest for the expanded tree, including every parent
/// directory, and the receipt that carries its proven bytes.
fn receipt(
    link: &str,
    entries: HashMap<String, Entry>,
) -> Result<RuntimeSourceReceipt, JavaRuntimeLookupError> {
    let mut directories = BTreeSet::new();
    let mut files = HashMap::new();
    let mut archive = RuntimeArchive::new();
    for (path, entry) in entries {
        let mut parent = path.as_str();
        while let Some((above, _)) = parent.rsplit_once('/') {
            directories.insert(above.to_owned());
            parent = above;
        }
        let file = match entry {
            Entry::Directory => {
                directories.insert(path);
                continue;
            }
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
                archive.insert(path.clone(), Arc::from(bytes));
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
    for directory in directories {
        files
            .entry(directory)
            .or_insert_with(|| ComponentManifestFile {
                kind: "directory".into(),
                executable: false,
                downloads: None,
                target: None,
            });
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
        Arc::new(archive),
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

    #[test]
    fn expansion_builds_a_complete_manifest_and_refuses_unsafe_paths() {
        let java = runtime_java_relative_path_in_archive();
        let archive = tar_gz(
            &[
                (&java, b"java", 0o755),
                ("jdk8u512-b09-jre/lib/rt.jar", b"classes", 0o644),
            ],
            Some(("jdk8u512-b09-jre/lib/link", "rt.jar")),
        );
        let entries = expand(&archive, Format::TarGz).unwrap();
        let source = receipt("https://example.invalid/jre.tar.gz", entries).unwrap();
        let manifest = source.manifest();
        let java_path = super::super::layout::runtime_java_relative_path();
        assert!(manifest.files[java_path].executable);
        assert_eq!(
            manifest.files[java_path.rsplit_once('/').unwrap().0].kind,
            "directory"
        );
        assert_eq!(manifest.files[&prefixed("lib/link")].kind, "link");
        assert_eq!(
            &*source.archive().unwrap()[&prefixed("lib/rt.jar")],
            b"classes"
        );

        let unsafe_archive = tar_gz_raw_path("jdk/../../escape", b"x");
        assert!(expand(&unsafe_archive, Format::TarGz).is_err());
        let missing = tar_gz(&[("jdk8u512-b09-jre/lib/rt.jar", b"classes", 0o644)], None);
        assert!(
            receipt(
                "https://example.invalid/jre.tar.gz",
                expand(&missing, Format::TarGz).unwrap()
            )
            .is_err()
        );
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
