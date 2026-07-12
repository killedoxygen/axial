use super::compose::LoaderProfileFragment;
use super::source::VerifiedLoaderSource;
use crate::artifact_path::ArtifactRelativePath;
use crate::download::DownloadError;
use crate::launch::{Library, maven_to_path};
use serde::Deserialize;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{Read, Write};
use std::path::Path;
use thiserror::Error;
use zip::{ZipArchive, ZipWriter, write::SimpleFileOptions};

#[cfg(not(test))]
const MAX_INSTALLER_PROFILE_ENTRY_BYTES: u64 = 8 << 20;
#[cfg(test)]
const MAX_INSTALLER_PROFILE_ENTRY_BYTES: u64 = 1024;
#[cfg(not(test))]
const MAX_INSTALLER_EMBEDDED_ENTRY_BYTES: u64 = 128 << 20;
#[cfg(test)]
const MAX_INSTALLER_EMBEDDED_ENTRY_BYTES: u64 = 1024;
#[cfg(not(test))]
const MAX_INSTALLER_ENTRY_COUNT: usize = 65_536;
#[cfg(test)]
const MAX_INSTALLER_ENTRY_COUNT: usize = 64;
#[cfg(not(test))]
const MAX_INSTALLER_EMBEDDED_TOTAL_BYTES: u64 = 512 << 20;
#[cfg(test)]
const MAX_INSTALLER_EMBEDDED_TOTAL_BYTES: u64 = 4096;

#[derive(Debug, Error)]
pub enum ForgeInstallerError {
    #[error("open installer zip: {0}")]
    Zip(#[from] zip::result::ZipError),
    #[error("installer io: {0}")]
    Io(#[from] std::io::Error),
    #[error("installer json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("version.json not found in installer")]
    MissingVersionJson,
    #[error("invalid installer entry path")]
    InvalidEntryPath,
    #[error("installer entry {name} is too large")]
    EntryTooLarge { name: String },
    #[error("installer contains too many entries")]
    TooManyEntries,
    #[error("installer embedded entries exceed the aggregate size limit")]
    EmbeddedEntriesTooLarge,
    #[error("installer contains a duplicate entry: {name}")]
    DuplicateEntry { name: String },
    #[error("installer declares a missing embedded entry: {name}")]
    MissingDeclaredEntry { name: String },
    #[error("installer contains conflicting embedded Maven artifacts")]
    ConflictingEmbeddedArtifact,
    #[error("installer contains conflicting declarations for library {name}")]
    ConflictingLibraryDeclaration { name: String },
    #[error("download failed: {0}")]
    Download(#[from] DownloadError),
}

#[derive(Debug)]
pub(crate) struct AuthenticatedForgeInstallerPlan {
    source: VerifiedLoaderSource,
    version_json: Vec<u8>,
    install_profile_json: Option<Vec<u8>>,
    libraries: Vec<Library>,
    embedded_maven_artifacts: Vec<AuthenticatedEmbeddedMavenArtifact>,
    strip_client_meta: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct AuthenticatedEmbeddedMavenArtifact {
    relative_path: ArtifactRelativePath,
    bytes: Vec<u8>,
}

impl AuthenticatedForgeInstallerPlan {
    pub(crate) fn source_bytes(&self) -> &[u8] {
        self.source.bytes()
    }

    pub(crate) fn version_json(&self) -> &[u8] {
        &self.version_json
    }

    pub(crate) fn install_profile_json(&self) -> Option<&[u8]> {
        self.install_profile_json.as_deref()
    }

    pub(crate) fn libraries(&self) -> &[Library] {
        &self.libraries
    }

    pub(crate) fn embedded_maven_artifacts(&self) -> &[AuthenticatedEmbeddedMavenArtifact] {
        &self.embedded_maven_artifacts
    }

    pub(crate) fn strip_client_meta(&self) -> bool {
        self.strip_client_meta
    }
}

impl AuthenticatedEmbeddedMavenArtifact {
    pub(crate) fn relative_path(&self) -> &ArtifactRelativePath {
        &self.relative_path
    }

    pub(crate) fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}

#[derive(Debug, Deserialize)]
struct LegacyInstallProfile {
    install: LegacyInstallData,
    #[serde(default)]
    minecraft: String,
    #[serde(rename = "versionInfo")]
    version_info: serde_json::Value,
}

#[derive(Debug, Deserialize)]
struct LegacyInstallData {
    path: String,
    #[serde(rename = "filePath")]
    file_path: String,
    target: String,
    #[serde(default)]
    minecraft: String,
    #[serde(default, rename = "stripMeta")]
    strip_meta: bool,
}

#[derive(Debug, Deserialize)]
struct InstallProfileLibraries {
    #[serde(default)]
    libraries: Vec<Library>,
}

pub(crate) fn plan_authenticated_installer(
    source: VerifiedLoaderSource,
) -> Result<AuthenticatedForgeInstallerPlan, ForgeInstallerError> {
    let mut archive = ZipArchive::new(std::io::Cursor::new(source.bytes()))?;
    if archive.len() > MAX_INSTALLER_ENTRY_COUNT {
        return Err(ForgeInstallerError::TooManyEntries);
    }

    let mut authored_version_json = None;
    let mut install_profile_json = None;
    let mut embedded = BTreeMap::new();
    let mut source_entry_counts = HashMap::new();
    let mut embedded_total = 0_u64;
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index)?;
        let name = entry.name().to_string();
        *source_entry_counts.entry(name.clone()).or_insert(0_usize) += 1;
        match name.as_str() {
            "version.json" => {
                if authored_version_json.is_some() {
                    return Err(ForgeInstallerError::DuplicateEntry { name });
                }
                authored_version_json = Some(read_bounded_entry(
                    &mut entry,
                    "version.json",
                    MAX_INSTALLER_PROFILE_ENTRY_BYTES,
                )?);
            }
            "install_profile.json" => {
                if install_profile_json.is_some() {
                    return Err(ForgeInstallerError::DuplicateEntry { name });
                }
                install_profile_json = Some(read_bounded_entry(
                    &mut entry,
                    "install_profile.json",
                    MAX_INSTALLER_PROFILE_ENTRY_BYTES,
                )?);
            }
            _ => {
                let Some(relative) = name.strip_prefix("maven/") else {
                    continue;
                };
                if relative.is_empty() || entry.is_dir() || relative.ends_with('/') {
                    continue;
                }
                let path = ArtifactRelativePath::new(relative)
                    .map_err(|_| ForgeInstallerError::InvalidEntryPath)?;
                let bytes = read_embedded_entry(&mut entry, relative, &mut embedded_total)?;
                if embedded.insert(path, bytes).is_some() {
                    return Err(ForgeInstallerError::DuplicateEntry { name });
                }
            }
        }
    }

    let effective_version_json = match (
        authored_version_json.as_ref(),
        install_profile_json.as_deref(),
    ) {
        (Some(version_json), _) => version_json.clone(),
        (None, Some(profile)) => extract_legacy_version_info(profile)?,
        (None, None) => return Err(ForgeInstallerError::MissingVersionJson),
    };
    let legacy_profile = install_profile_json
        .as_deref()
        .and_then(|profile| serde_json::from_slice::<LegacyInstallProfile>(profile).ok());
    if let Some(profile) = legacy_profile.as_ref() {
        add_legacy_root_artifact(
            &mut archive,
            profile,
            &source_entry_counts,
            &mut embedded,
            &mut embedded_total,
        )?;
    }

    let version_fragment =
        serde_json::from_slice::<LoaderProfileFragment>(&effective_version_json)?;
    let install_info = install_profile_json
        .as_deref()
        .map(serde_json::from_slice::<InstallProfileLibraries>)
        .transpose()?;
    let libraries = merge_libraries_by_name(
        &version_fragment.libraries,
        install_info
            .as_ref()
            .map(|info| info.libraries.as_slice())
            .unwrap_or(&[]),
    )?;
    drop(archive);

    Ok(AuthenticatedForgeInstallerPlan {
        source,
        version_json: effective_version_json,
        install_profile_json,
        libraries,
        embedded_maven_artifacts: embedded
            .into_iter()
            .map(
                |(relative_path, bytes)| AuthenticatedEmbeddedMavenArtifact {
                    relative_path,
                    bytes,
                },
            )
            .collect(),
        strip_client_meta: legacy_profile.is_some_and(|profile| profile.install.strip_meta),
    })
}

fn merge_libraries_by_name(
    primary: &[Library],
    secondary: &[Library],
) -> Result<Vec<Library>, ForgeInstallerError> {
    let mut seen = HashMap::new();
    let mut merged = Vec::with_capacity(primary.len() + secondary.len());

    for library in primary.iter().chain(secondary.iter()) {
        if let Some(existing) = seen.get(&library.name) {
            if existing != library {
                return Err(ForgeInstallerError::ConflictingLibraryDeclaration {
                    name: library.name.clone(),
                });
            }
        } else {
            seen.insert(library.name.clone(), library.clone());
            merged.push(library.clone());
        }
    }

    Ok(merged)
}

fn add_legacy_root_artifact(
    archive: &mut ZipArchive<std::io::Cursor<&[u8]>>,
    profile: &LegacyInstallProfile,
    source_entry_counts: &HashMap<String, usize>,
    embedded: &mut BTreeMap<ArtifactRelativePath, Vec<u8>>,
    embedded_total: &mut u64,
) -> Result<(), ForgeInstallerError> {
    let minecraft = legacy_profile_minecraft(profile);
    let Some(normalized_library) = normalize_legacy_forge_library(
        &profile.install.path,
        &profile.install.file_path,
        minecraft,
    ) else {
        return Err(ForgeInstallerError::InvalidEntryPath);
    };
    let artifact_path = ArtifactRelativePath::from_path(&maven_to_path(&normalized_library))
        .map_err(|_| ForgeInstallerError::InvalidEntryPath)?;

    let entry_name = profile.install.file_path.trim();
    if entry_name.is_empty() || entry_name.contains('/') || entry_name.contains('\\') {
        return Err(ForgeInstallerError::InvalidEntryPath);
    }
    match source_entry_counts.get(entry_name).copied() {
        Some(1) => {}
        Some(_) => {
            return Err(ForgeInstallerError::DuplicateEntry {
                name: entry_name.to_string(),
            });
        }
        None => {
            return Err(ForgeInstallerError::MissingDeclaredEntry {
                name: entry_name.to_string(),
            });
        }
    }
    let mut entry = archive.by_name(entry_name)?;
    let bytes = read_embedded_entry(&mut entry, entry_name, embedded_total)?;
    let bytes = if profile.install.strip_meta {
        strip_signed_metadata_in_memory(&bytes, entry_name)?
    } else {
        bytes
    };
    if embedded.insert(artifact_path, bytes).is_some() {
        return Err(ForgeInstallerError::ConflictingEmbeddedArtifact);
    }
    Ok(())
}

fn strip_signed_metadata_in_memory(
    data: &[u8],
    name: &str,
) -> Result<Vec<u8>, ForgeInstallerError> {
    let mut source = ZipArchive::new(std::io::Cursor::new(data))?;
    if source.len() > MAX_INSTALLER_ENTRY_COUNT {
        return Err(ForgeInstallerError::TooManyEntries);
    }
    let mut writer = ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let mut seen = HashSet::new();
    let mut total = 0_u64;
    for index in 0..source.len() {
        let mut entry = source.by_index(index)?;
        let entry_name = entry.name().to_string();
        if legacy_signed_metadata_entry_is_skipped(&entry_name) {
            continue;
        }
        if !seen.insert(entry_name.clone()) {
            return Err(ForgeInstallerError::DuplicateEntry { name: entry_name });
        }
        if entry.is_dir() || entry_name.ends_with('/') {
            writer.add_directory(&entry_name, SimpleFileOptions::default())?;
            continue;
        }

        let bytes = read_embedded_entry(&mut entry, name, &mut total)?;
        writer.start_file(&entry_name, SimpleFileOptions::default())?;
        writer.write_all(&bytes)?;
    }
    let output = writer.finish()?.into_inner();
    if output.len() as u64 > MAX_INSTALLER_EMBEDDED_ENTRY_BYTES {
        return Err(ForgeInstallerError::EntryTooLarge {
            name: name.to_string(),
        });
    }
    Ok(output)
}

fn legacy_signed_metadata_entry_is_skipped(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    upper == "META-INF/MANIFEST.MF"
        || upper.ends_with(".SF")
        || upper.ends_with(".RSA")
        || upper.ends_with(".DSA")
}

fn read_bounded_entry(
    file: &mut zip::read::ZipFile<'_>,
    name: &str,
    max_bytes: u64,
) -> Result<Vec<u8>, ForgeInstallerError> {
    if file.size() > max_bytes {
        return Err(ForgeInstallerError::EntryTooLarge {
            name: name.to_string(),
        });
    }
    let mut data = Vec::new();
    let mut bounded = (&mut *file).take(max_bytes + 1);
    bounded.read_to_end(&mut data)?;
    if data.len() as u64 > max_bytes || data.len() as u64 != file.size() {
        return Err(ForgeInstallerError::EntryTooLarge {
            name: name.to_string(),
        });
    }
    Ok(data)
}

fn read_embedded_entry(
    file: &mut zip::read::ZipFile<'_>,
    name: &str,
    total: &mut u64,
) -> Result<Vec<u8>, ForgeInstallerError> {
    let bytes = read_bounded_entry(file, name, MAX_INSTALLER_EMBEDDED_ENTRY_BYTES)?;
    *total = total
        .checked_add(bytes.len() as u64)
        .ok_or(ForgeInstallerError::EmbeddedEntriesTooLarge)?;
    if *total > MAX_INSTALLER_EMBEDDED_TOTAL_BYTES {
        return Err(ForgeInstallerError::EmbeddedEntriesTooLarge);
    }
    Ok(bytes)
}

fn extract_legacy_version_info(install_profile: &[u8]) -> Result<Vec<u8>, ForgeInstallerError> {
    let profile = serde_json::from_slice::<LegacyInstallProfile>(install_profile)?;
    let minecraft = legacy_profile_minecraft(&profile).to_string();
    let mut version_info = profile.version_info;

    if let Some(version_id) = normalize_legacy_forge_version_id(&profile.install.path, &minecraft)
        .or_else(|| (!profile.install.target.is_empty()).then(|| profile.install.target.clone()))
    {
        version_info["id"] = serde_json::Value::String(version_id);
    }

    if let Some(normalized_library) = normalize_legacy_forge_library(
        &profile.install.path,
        &profile.install.file_path,
        &minecraft,
    ) && let Some(libraries) = version_info
        .get_mut("libraries")
        .and_then(|value| value.as_array_mut())
    {
        for library in libraries.iter_mut() {
            if library.get("name").and_then(|value| value.as_str())
                == Some(profile.install.path.as_str())
            {
                library["name"] = serde_json::Value::String(normalized_library.clone());
                break;
            }
        }
    }

    Ok(serde_json::to_vec(&version_info)?)
}

fn legacy_profile_minecraft(profile: &LegacyInstallProfile) -> &str {
    let install_minecraft = profile.install.minecraft.trim();
    if install_minecraft.is_empty() {
        profile.minecraft.trim()
    } else {
        install_minecraft
    }
}

fn normalize_legacy_forge_library(path: &str, file_path: &str, minecraft: &str) -> Option<String> {
    let mut parts = path.split(':');
    let group = parts.next()?;
    let artifact = parts.next()?;
    let version = parts.next()?;
    if parts.next().is_some() {
        return None;
    }

    let filename = Path::new(file_path).file_stem()?.to_string_lossy();
    if artifact == "minecraftforge" && !minecraft.trim().is_empty() {
        let classifier = if filename.contains("-universal-") {
            "universal"
        } else if filename.contains("-client-") {
            "client"
        } else if filename.contains("-server-") {
            "server"
        } else {
            return None;
        };
        return Some(format!("{group}:forge:{minecraft}-{version}:{classifier}"));
    }

    let prefix = format!("{artifact}-{version}-");
    let classifier = filename.strip_prefix(&prefix)?;
    if classifier.is_empty() {
        return None;
    }
    Some(format!("{group}:{artifact}:{version}:{classifier}"))
}

fn normalize_legacy_forge_version_id(path: &str, minecraft: &str) -> Option<String> {
    let mut parts = path.split(':');
    let _group = parts.next()?;
    let artifact = parts.next()?;
    let version = parts.next()?;
    if parts.next().is_some() {
        return None;
    }
    if artifact == "minecraftforge" && !minecraft.trim().is_empty() {
        return Some(format!("{minecraft}-forge-{version}"));
    }
    let index = version.find('-')?;
    if index == 0 || index + 1 >= version.len() {
        return None;
    }
    Some(format!(
        "{}-forge-{}",
        &version[..index],
        &version[index + 1..]
    ))
}

#[cfg(test)]
mod tests {
    use super::{
        AuthenticatedForgeInstallerPlan, ForgeInstallerError, MAX_INSTALLER_EMBEDDED_ENTRY_BYTES,
        MAX_INSTALLER_PROFILE_ENTRY_BYTES, merge_libraries_by_name, normalize_legacy_forge_library,
        normalize_legacy_forge_version_id, plan_authenticated_installer,
    };
    use crate::launch::Library;
    use crate::loaders::source::VerifiedLoaderSource;
    use std::io::{Cursor, Write};
    use std::time::{SystemTime, UNIX_EPOCH};
    use zip::write::SimpleFileOptions;

    #[test]
    fn normalizes_legacy_forge_version_id() {
        assert_eq!(
            normalize_legacy_forge_version_id("net.minecraftforge:forge:1.2.4-2.0.0.68", ""),
            Some("1.2.4-forge-2.0.0.68".to_string())
        );
    }

    #[test]
    fn normalizes_legacy_forge_library_classifier() {
        assert_eq!(
            normalize_legacy_forge_library(
                "net.minecraftforge:forge:1.2.4-2.0.0.68",
                "forge-1.2.4-2.0.0.68-universal.zip",
                ""
            ),
            Some("net.minecraftforge:forge:1.2.4-2.0.0.68:universal".to_string())
        );
    }

    #[test]
    fn normalizes_minecraftforge_legacy_coordinates() {
        assert_eq!(
            normalize_legacy_forge_version_id(
                "net.minecraftforge:minecraftforge:9.11.1.1345",
                "1.6.4"
            ),
            Some("1.6.4-forge-9.11.1.1345".to_string())
        );
        assert_eq!(
            normalize_legacy_forge_library(
                "net.minecraftforge:minecraftforge:9.11.1.1345",
                "minecraftforge-universal-1.6.4-9.11.1.1345.jar",
                "1.6.4"
            ),
            Some("net.minecraftforge:forge:1.6.4-9.11.1.1345:universal".to_string())
        );
    }

    #[test]
    fn pure_plan_retains_legacy_root_forge_library() {
        let install_profile = br#"{
            "versionInfo": {
                "id": "1.6.4-Forge9.11.1.1345",
                "libraries": [
                    { "name": "net.minecraftforge:minecraftforge:9.11.1.1345" }
                ]
            },
            "install": {
                "path": "net.minecraftforge:minecraftforge:9.11.1.1345",
                "filePath": "minecraftforge-universal-1.6.4-9.11.1.1345.jar",
                "target": "1.6.4-Forge9.11.1.1345",
                "minecraft": "1.6.4"
            }
        }"#;
        let jar = zip_with_entries(&[
            ("install_profile.json", install_profile.as_slice()),
            (
                "minecraftforge-universal-1.6.4-9.11.1.1345.jar",
                b"forge universal",
            ),
        ]);

        let plan = plan(&jar);
        let artifact = embedded_artifact(
            &plan,
            "net/minecraftforge/forge/1.6.4-9.11.1.1345/forge-1.6.4-9.11.1.1345-universal.jar",
        );
        assert_eq!(artifact, b"forge universal");
    }

    #[test]
    fn pure_plan_strips_legacy_root_forge_library_meta_in_memory() {
        let install_profile = br#"{
            "versionInfo": {
                "id": "1.5.2-Forge7.8.1.738",
                "libraries": [
                    { "name": "net.minecraftforge:minecraftforge:7.8.1.738" }
                ]
            },
            "install": {
                "path": "net.minecraftforge:minecraftforge:7.8.1.738",
                "filePath": "minecraftforge-universal-1.5.2-7.8.1.738.jar",
                "target": "1.5.2-Forge7.8.1.738",
                "minecraft": "1.5.2",
                "stripMeta": true
            }
        }"#;
        let forge_jar = zip_with_entries(&[
            ("META-INF/MANIFEST.MF", b"signed manifest".as_slice()),
            ("META-INF/FORGE.SF", b"signature".as_slice()),
            ("META-INF/FORGE.DSA", b"signature".as_slice()),
            ("net/minecraft/client/Minecraft.class", b"class".as_slice()),
        ]);
        let jar = zip_with_entries(&[
            ("install_profile.json", install_profile.as_slice()),
            (
                "minecraftforge-universal-1.5.2-7.8.1.738.jar",
                forge_jar.as_slice(),
            ),
        ]);
        let plan = plan(&jar);
        let installed_jar = embedded_artifact(
            &plan,
            "net/minecraftforge/forge/1.5.2-7.8.1.738/forge-1.5.2-7.8.1.738-universal.jar",
        );
        assert!(zip_contains(
            installed_jar,
            "net/minecraft/client/Minecraft.class"
        ));
        assert!(!zip_contains(installed_jar, "META-INF/MANIFEST.MF"));
        assert!(!zip_contains(installed_jar, "META-INF/FORGE.SF"));
        assert!(!zip_contains(installed_jar, "META-INF/FORGE.DSA"));
    }

    #[test]
    fn pure_plan_retains_modern_embedded_maven_entry() {
        let version_json = br#"{
            "id": "1.21.1-forge-52.1.0",
            "libraries": []
        }"#;
        let install_profile = br#"{
            "spec": 1,
            "profile": "forge",
            "version": "1.21.1-52.1.0",
            "libraries": [],
            "processors": []
        }"#;
        let jar = zip_with_entries(&[
            ("version.json", version_json.as_slice()),
            ("install_profile.json", install_profile.as_slice()),
            (
                "maven/net/minecraftforge/forge/1.21.1-52.1.0/forge-1.21.1-52.1.0-shim.jar",
                b"shim",
            ),
        ]);

        let plan = plan(&jar);
        assert_eq!(
            embedded_artifact(
                &plan,
                "net/minecraftforge/forge/1.21.1-52.1.0/forge-1.21.1-52.1.0-shim.jar",
            ),
            b"shim"
        );
    }

    #[test]
    fn pure_plan_retains_exact_profiles_and_source_bytes() {
        let version_json = br#"{
            "id": "1.21.1-forge-52.1.0",
            "libraries": [{"name":"example:version-lib:1.0"}]
        }"#;
        let install_profile = br#"{
            "libraries": [{"name":"example:installer-lib:1.0"}],
            "processors": [{"args":["{ROOT}/output.jar","{INPUT}"]}],
            "data": {
                "INPUT": {"client":"/data/input.bin"},
                "LIB": {"client":"[example:installer-lib:1.0]"}
            }
        }"#;
        let jar = zip_with_entries(&[
            ("version.json", version_json.as_slice()),
            ("install_profile.json", install_profile.as_slice()),
            ("maven/example/embedded/1.0/embedded-1.0.jar", b"jar"),
        ]);

        let plan = plan(&jar);
        assert_eq!(plan.source_bytes(), jar);
        assert_eq!(plan.version_json(), version_json);
        assert_eq!(
            plan.install_profile_json(),
            Some(install_profile.as_slice())
        );
        assert_eq!(plan.libraries().len(), 2);
    }

    #[test]
    fn pure_plan_rejects_duplicate_and_unsafe_maven_paths() {
        let version_json = br#"{"id":"forge","libraries":[]}"#;
        let duplicate = zip_with_entries(&[
            ("version.json", version_json.as_slice()),
            ("maven/example/mod.jar", b"first"),
            (r"maven/example\mod.jar", b"second"),
        ]);
        assert!(matches!(
            plan_authenticated_installer(VerifiedLoaderSource::from_test_bytes(duplicate)),
            Err(ForgeInstallerError::DuplicateEntry { .. })
        ));

        let unsafe_path = zip_with_entries(&[
            ("version.json", version_json.as_slice()),
            ("maven/../outside.jar", b"outside"),
        ]);
        assert!(matches!(
            plan_authenticated_installer(VerifiedLoaderSource::from_test_bytes(unsafe_path)),
            Err(ForgeInstallerError::InvalidEntryPath)
        ));
    }

    #[test]
    fn pure_plan_rejects_conflicting_legacy_and_maven_artifacts() {
        let install_profile = br#"{
            "versionInfo": {
                "id": "1.6.4-Forge9.11.1.1345",
                "libraries": [{"name":"net.minecraftforge:minecraftforge:9.11.1.1345"}]
            },
            "install": {
                "path": "net.minecraftforge:minecraftforge:9.11.1.1345",
                "filePath": "minecraftforge-universal-1.6.4-9.11.1.1345.jar",
                "target": "1.6.4-Forge9.11.1.1345",
                "minecraft": "1.6.4"
            }
        }"#;
        let jar = zip_with_entries(&[
            ("install_profile.json", install_profile.as_slice()),
            (
                "minecraftforge-universal-1.6.4-9.11.1.1345.jar",
                b"legacy root",
            ),
            (
                "maven/net/minecraftforge/forge/1.6.4-9.11.1.1345/forge-1.6.4-9.11.1.1345-universal.jar",
                b"maven copy",
            ),
        ]);

        assert!(matches!(
            plan_authenticated_installer(VerifiedLoaderSource::from_test_bytes(jar)),
            Err(ForgeInstallerError::ConflictingEmbeddedArtifact)
        ));
    }

    #[test]
    fn pure_plan_has_no_filesystem_effects_and_enforces_aggregate_bounds() {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|value| value.as_nanos())
            .unwrap_or_default();
        let nonexistent = std::env::temp_dir().join(format!("axial-pure-installer-{nanos:x}"));
        assert!(!nonexistent.exists());

        let version_json = br#"{"id":"forge","libraries":[]}"#;
        let valid = zip_with_entries(&[
            ("version.json", version_json.as_slice()),
            ("maven/example/mod.jar", b"mod"),
        ]);
        plan(&valid);
        assert!(!nonexistent.exists());

        let aggregate = zip_with_generated_maven_entries(5, 900);
        assert!(matches!(
            plan_authenticated_installer(VerifiedLoaderSource::from_test_bytes(aggregate)),
            Err(ForgeInstallerError::EmbeddedEntriesTooLarge)
        ));
        let too_many = zip_with_generated_maven_entries(super::MAX_INSTALLER_ENTRY_COUNT, 0);
        assert!(matches!(
            plan_authenticated_installer(VerifiedLoaderSource::from_test_bytes(too_many)),
            Err(ForgeInstallerError::TooManyEntries)
        ));
    }

    #[test]
    fn pure_plan_reports_legacy_strip_meta() {
        let install_profile = br#"{
            "versionInfo": {
                "id": "1.5.2-Forge7.8.1.738",
                "mainClass": "net.minecraft.launchwrapper.Launch",
                "minecraftArguments": "${auth_player_name} ${auth_session}",
                "assetIndex": { "id": "legacy" },
                "libraries": [
                    { "name": "net.minecraftforge:minecraftforge:7.8.1.738" }
                ]
            },
            "install": {
                "path": "net.minecraftforge:minecraftforge:7.8.1.738",
                "filePath": "minecraftforge-universal-1.5.2-7.8.1.738.jar",
                "target": "1.5.2-Forge7.8.1.738",
                "minecraft": "1.5.2",
                "stripMeta": true
            }
        }"#;
        let forge_jar = zip_with_entries(&[("example/Class.class", b"class".as_slice())]);
        let jar = zip_with_entries(&[
            ("install_profile.json", install_profile.as_slice()),
            (
                "minecraftforge-universal-1.5.2-7.8.1.738.jar",
                forge_jar.as_slice(),
            ),
        ]);

        let extracted = plan(&jar);

        assert!(extracted.strip_client_meta());
    }

    #[test]
    fn merge_libraries_by_name_keeps_distinct_versions() {
        let merged = merge_libraries_by_name(
            &[Library {
                name: "net.sf.jopt-simple:jopt-simple:5.0.4".to_string(),
                ..Library::default()
            }],
            &[Library {
                name: "net.sf.jopt-simple:jopt-simple:6.0-alpha-3".to_string(),
                ..Library::default()
            }],
        )
        .expect("distinct library declarations");

        assert_eq!(
            merged
                .into_iter()
                .map(|library| library.name)
                .collect::<Vec<_>>(),
            vec![
                "net.sf.jopt-simple:jopt-simple:5.0.4".to_string(),
                "net.sf.jopt-simple:jopt-simple:6.0-alpha-3".to_string()
            ]
        );
    }

    #[test]
    fn merge_libraries_rejects_same_coordinate_declaration_drift() {
        let primary = Library {
            name: "example:library:1.0".to_string(),
            sha1: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
            ..Library::default()
        };
        let mut conflicting = primary.clone();
        conflicting.sha1 = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_string();

        assert!(matches!(
            merge_libraries_by_name(&[primary], &[conflicting]),
            Err(ForgeInstallerError::ConflictingLibraryDeclaration { .. })
        ));
    }

    #[test]
    fn pure_plan_rejects_oversized_profile_entry() {
        let jar = zip_with_entry(
            "install_profile.json",
            vec![b' '; (MAX_INSTALLER_PROFILE_ENTRY_BYTES + 1) as usize],
        );

        let error = plan_authenticated_installer(VerifiedLoaderSource::from_test_bytes(jar))
            .expect_err("oversized install profile should fail");

        assert!(
            matches!(error, ForgeInstallerError::EntryTooLarge { name } if name == "install_profile.json")
        );
    }

    #[test]
    fn pure_plan_rejects_oversized_maven_entry_without_effects() {
        let jar = zip_with_entry(
            "maven/example/mod.jar",
            vec![b'j'; (MAX_INSTALLER_EMBEDDED_ENTRY_BYTES + 1) as usize],
        );

        let error = plan_authenticated_installer(VerifiedLoaderSource::from_test_bytes(jar))
            .expect_err("oversized maven entry should fail");

        assert!(
            matches!(error, ForgeInstallerError::EntryTooLarge { name } if name == "example/mod.jar")
        );
    }

    fn zip_with_entry(name: &str, bytes: Vec<u8>) -> Vec<u8> {
        zip_with_entries(&[(name, bytes.as_slice())])
    }

    fn zip_with_entries(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut cursor = Cursor::new(Vec::new());
        {
            let mut writer = zip::ZipWriter::new(&mut cursor);
            for (name, bytes) in entries {
                writer
                    .start_file(*name, SimpleFileOptions::default())
                    .expect("start zip file");
                writer.write_all(bytes).expect("write zip file");
            }
            writer.finish().expect("finish zip");
        }
        cursor.into_inner()
    }

    fn zip_with_generated_maven_entries(count: usize, bytes_per_entry: usize) -> Vec<u8> {
        let mut cursor = Cursor::new(Vec::new());
        {
            let mut writer = zip::ZipWriter::new(&mut cursor);
            writer
                .start_file("version.json", SimpleFileOptions::default())
                .expect("start version json");
            writer
                .write_all(br#"{"id":"forge","libraries":[]}"#)
                .expect("write version json");
            for index in 0..count {
                writer
                    .start_file(
                        format!("maven/example/artifact-{index}.jar"),
                        SimpleFileOptions::default(),
                    )
                    .expect("start Maven entry");
                writer
                    .write_all(&vec![b'x'; bytes_per_entry])
                    .expect("write Maven entry");
            }
            writer.finish().expect("finish generated installer");
        }
        cursor.into_inner()
    }

    fn zip_contains(bytes: &[u8], name: &str) -> bool {
        let mut archive = zip::ZipArchive::new(Cursor::new(bytes)).expect("zip archive");
        archive.by_name(name).is_ok()
    }

    fn plan(bytes: &[u8]) -> AuthenticatedForgeInstallerPlan {
        plan_authenticated_installer(VerifiedLoaderSource::from_test_bytes(bytes.to_vec()))
            .expect("authenticated installer plan")
    }

    fn embedded_artifact<'a>(plan: &'a AuthenticatedForgeInstallerPlan, path: &str) -> &'a [u8] {
        plan.embedded_maven_artifacts()
            .iter()
            .find(|artifact| artifact.relative_path().as_str() == path)
            .expect("embedded Maven artifact")
            .bytes()
    }
}
