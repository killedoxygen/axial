use super::compose::LoaderProfileFragment;
use crate::download::DownloadError;
use crate::launch::{Library, maven_to_path};
use crate::paths::libraries_dir;
use serde::Deserialize;
use std::collections::HashSet;
use std::fs;
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
    #[error("download failed: {0}")]
    Download(#[from] DownloadError),
}

#[derive(Debug)]
pub struct ExtractedForgeInstaller {
    pub version_fragment: LoaderProfileFragment,
    pub install_profile_json: Option<Vec<u8>>,
    pub version_id: String,
    pub libraries: Vec<Library>,
    pub strip_client_meta: bool,
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

pub fn extract_installer(jar_data: &[u8]) -> Result<ExtractedForgeInstaller, ForgeInstallerError> {
    let mut archive = ZipArchive::new(std::io::Cursor::new(jar_data))?;
    let version_json = read_optional_entry(&mut archive, "version.json")?;
    let install_profile = read_optional_entry(&mut archive, "install_profile.json")?;

    let version_json = match (version_json, install_profile.as_deref()) {
        (Some(version_json), _) => version_json,
        (None, Some(profile)) => extract_legacy_version_info(profile)?,
        (None, None) => return Err(ForgeInstallerError::MissingVersionJson),
    };

    let strip_client_meta = install_profile
        .as_deref()
        .and_then(|profile| serde_json::from_slice::<LegacyInstallProfile>(profile).ok())
        .is_some_and(|profile| profile.install.strip_meta);
    let version = serde_json::from_slice::<LoaderProfileFragment>(&version_json)?;
    let install_info = install_profile
        .as_deref()
        .map(serde_json::from_slice::<InstallProfileLibraries>)
        .transpose()?;
    let libraries = merge_libraries_by_name(
        &version.libraries,
        install_info
            .as_ref()
            .map(|info| info.libraries.as_slice())
            .unwrap_or(&[]),
    );

    Ok(ExtractedForgeInstaller {
        install_profile_json: install_profile,
        version_id: version.id.clone(),
        version_fragment: version,
        libraries,
        strip_client_meta,
    })
}

fn merge_libraries_by_name(primary: &[Library], secondary: &[Library]) -> Vec<Library> {
    let mut seen = HashSet::new();
    let mut merged = Vec::with_capacity(primary.len() + secondary.len());

    for library in primary.iter().chain(secondary.iter()) {
        if !seen.insert(library.name.clone()) {
            continue;
        }
        merged.push(library.clone());
    }

    merged
}

pub fn extract_maven_entries(jar_data: &[u8], mc_dir: &Path) -> Result<(), ForgeInstallerError> {
    let mut archive = ZipArchive::new(std::io::Cursor::new(jar_data))?;
    let libraries_dir = libraries_dir(mc_dir);

    for index in 0..archive.len() {
        let mut file = archive.by_index(index)?;
        let Some(relative) = file.name().strip_prefix("maven/") else {
            continue;
        };
        let relative = relative.to_string();
        if relative.is_empty() || relative.ends_with('/') {
            continue;
        }

        let normalized = relative.replace('/', std::path::MAIN_SEPARATOR_STR);
        let relative_path = Path::new(&normalized);
        if relative_path.is_absolute()
            || relative_path
                .components()
                .any(|component| matches!(component, std::path::Component::ParentDir))
        {
            return Err(ForgeInstallerError::InvalidEntryPath);
        }
        if file.size() > MAX_INSTALLER_EMBEDDED_ENTRY_BYTES {
            return Err(ForgeInstallerError::EntryTooLarge {
                name: relative.to_string(),
            });
        }

        let destination = libraries_dir.join(relative_path);
        if let Ok(metadata) = fs::metadata(&destination)
            && metadata.len() == file.size()
        {
            continue;
        }

        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)?;
        }
        copy_installer_entry(&mut file, &destination, &relative)?;
    }

    let Some(install_profile) = read_optional_entry(&mut archive, "install_profile.json")? else {
        return Ok(());
    };
    extract_legacy_root_library_entry(&mut archive, &install_profile, &libraries_dir)?;
    Ok(())
}

fn extract_legacy_root_library_entry(
    archive: &mut ZipArchive<std::io::Cursor<&[u8]>>,
    install_profile: &[u8],
    libraries_dir: &Path,
) -> Result<(), ForgeInstallerError> {
    let Ok(profile) = serde_json::from_slice::<LegacyInstallProfile>(install_profile) else {
        return Ok(());
    };
    let minecraft = legacy_profile_minecraft(&profile);
    let Some(normalized_library) = normalize_legacy_forge_library(
        &profile.install.path,
        &profile.install.file_path,
        minecraft,
    ) else {
        return Ok(());
    };
    let artifact_path = maven_to_path(&normalized_library);
    if artifact_path.as_os_str().is_empty()
        || artifact_path.is_absolute()
        || artifact_path
            .components()
            .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        return Ok(());
    }

    let entry_name = profile.install.file_path.trim();
    if entry_name.is_empty() || entry_name.contains('/') || entry_name.contains('\\') {
        return Ok(());
    }
    let Ok(mut file) = archive.by_name(entry_name) else {
        return Ok(());
    };

    let destination = libraries_dir.join(artifact_path);
    if !profile.install.strip_meta
        && let Ok(metadata) = fs::metadata(&destination)
        && metadata.len() == file.size()
    {
        return Ok(());
    }
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
    }
    if profile.install.strip_meta {
        copy_stripped_zip_installer_entry(&mut file, &destination, entry_name)
    } else {
        copy_installer_entry(&mut file, &destination, entry_name)
    }
}

fn copy_installer_entry(
    file: &mut zip::read::ZipFile<'_>,
    destination: &Path,
    name: &str,
) -> Result<(), ForgeInstallerError> {
    let mut output = fs::File::create(destination)?;
    let mut bounded = (&mut *file).take(MAX_INSTALLER_EMBEDDED_ENTRY_BYTES + 1);
    let copied = std::io::copy(&mut bounded, &mut output)?;
    if copied > MAX_INSTALLER_EMBEDDED_ENTRY_BYTES {
        let _ = fs::remove_file(destination);
        return Err(ForgeInstallerError::EntryTooLarge {
            name: name.to_string(),
        });
    }
    Ok(())
}

fn copy_stripped_zip_installer_entry(
    file: &mut zip::read::ZipFile<'_>,
    destination: &Path,
    name: &str,
) -> Result<(), ForgeInstallerError> {
    let mut data = Vec::new();
    let mut bounded = (&mut *file).take(MAX_INSTALLER_EMBEDDED_ENTRY_BYTES + 1);
    bounded.read_to_end(&mut data)?;
    if data.len() as u64 > MAX_INSTALLER_EMBEDDED_ENTRY_BYTES {
        let _ = fs::remove_file(destination);
        return Err(ForgeInstallerError::EntryTooLarge {
            name: name.to_string(),
        });
    }

    let mut source = ZipArchive::new(std::io::Cursor::new(data))?;
    let output = fs::File::create(destination)?;
    let mut writer = ZipWriter::new(output);
    for index in 0..source.len() {
        let mut entry = source.by_index(index)?;
        let entry_name = entry.name().to_string();
        if legacy_signed_metadata_entry_is_skipped(&entry_name) {
            continue;
        }
        if entry.is_dir() || entry_name.ends_with('/') {
            writer.add_directory(&entry_name, SimpleFileOptions::default())?;
            continue;
        }

        let mut bytes = Vec::new();
        entry.read_to_end(&mut bytes)?;
        writer.start_file(&entry_name, SimpleFileOptions::default())?;
        writer.write_all(&bytes)?;
    }
    writer.finish()?;
    Ok(())
}

fn legacy_signed_metadata_entry_is_skipped(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    upper == "META-INF/MANIFEST.MF"
        || upper.ends_with(".SF")
        || upper.ends_with(".RSA")
        || upper.ends_with(".DSA")
}

fn read_optional_entry(
    archive: &mut ZipArchive<std::io::Cursor<&[u8]>>,
    name: &str,
) -> Result<Option<Vec<u8>>, ForgeInstallerError> {
    let Ok(mut file) = archive.by_name(name) else {
        return Ok(None);
    };
    if file.size() > MAX_INSTALLER_PROFILE_ENTRY_BYTES {
        return Err(ForgeInstallerError::EntryTooLarge {
            name: name.to_string(),
        });
    }
    let mut data = Vec::new();
    let mut bounded = (&mut file).take(MAX_INSTALLER_PROFILE_ENTRY_BYTES + 1);
    bounded.read_to_end(&mut data)?;
    if data.len() as u64 > MAX_INSTALLER_PROFILE_ENTRY_BYTES {
        return Err(ForgeInstallerError::EntryTooLarge {
            name: name.to_string(),
        });
    }
    Ok(Some(data))
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
        ForgeInstallerError, MAX_INSTALLER_EMBEDDED_ENTRY_BYTES, MAX_INSTALLER_PROFILE_ENTRY_BYTES,
        extract_installer, extract_maven_entries, merge_libraries_by_name,
        normalize_legacy_forge_library, normalize_legacy_forge_version_id,
    };
    use crate::launch::Library;
    use std::fs;
    use std::io::{Cursor, Write};
    use std::path::PathBuf;
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
    fn extract_maven_entries_copies_legacy_root_forge_library() {
        let root = test_root("legacy-root-forge-library");
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

        extract_maven_entries(&jar, &root).expect("extract legacy root library");

        let artifact = root
            .join("libraries")
            .join("net")
            .join("minecraftforge")
            .join("forge")
            .join("1.6.4-9.11.1.1345")
            .join("forge-1.6.4-9.11.1.1345-universal.jar");
        assert_eq!(
            fs::read(&artifact).expect("read extracted artifact"),
            b"forge universal"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn extract_maven_entries_strips_legacy_root_forge_library_meta_when_requested() {
        let root = test_root("legacy-root-forge-library-strip-meta");
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
        let artifact = root
            .join("libraries")
            .join("net")
            .join("minecraftforge")
            .join("forge")
            .join("1.5.2-7.8.1.738")
            .join("forge-1.5.2-7.8.1.738-universal.jar");
        fs::create_dir_all(artifact.parent().expect("artifact parent"))
            .expect("create stale artifact parent");
        fs::write(&artifact, &forge_jar).expect("write stale verbatim artifact");

        extract_maven_entries(&jar, &root).expect("extract legacy root library");

        let installed_jar = fs::read(&artifact).expect("read extracted artifact");
        assert!(zip_contains(
            &installed_jar,
            "net/minecraft/client/Minecraft.class"
        ));
        assert!(!zip_contains(&installed_jar, "META-INF/MANIFEST.MF"));
        assert!(!zip_contains(&installed_jar, "META-INF/FORGE.SF"));
        assert!(!zip_contains(&installed_jar, "META-INF/FORGE.DSA"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn extract_maven_entries_ignores_modern_install_profile_for_legacy_root_library() {
        let root = test_root("modern-install-profile-no-legacy-root-library");
        let install_profile = br#"{
            "spec": 1,
            "profile": "forge",
            "version": "1.21.1-52.1.0",
            "libraries": [],
            "processors": []
        }"#;
        let jar = zip_with_entries(&[
            ("install_profile.json", install_profile.as_slice()),
            (
                "maven/net/minecraftforge/forge/1.21.1-52.1.0/forge-1.21.1-52.1.0-shim.jar",
                b"shim",
            ),
        ]);

        extract_maven_entries(&jar, &root).expect("extract modern maven entries");

        let artifact = root
            .join("libraries")
            .join("net")
            .join("minecraftforge")
            .join("forge")
            .join("1.21.1-52.1.0")
            .join("forge-1.21.1-52.1.0-shim.jar");
        assert_eq!(fs::read(&artifact).expect("read shim"), b"shim");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn extract_installer_reports_legacy_strip_meta() {
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
        let jar = zip_with_entries(&[("install_profile.json", install_profile.as_slice())]);

        let extracted = extract_installer(&jar).expect("extract legacy installer");

        assert!(extracted.strip_client_meta);
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
        );

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
    fn extract_installer_rejects_oversized_profile_entry() {
        let jar = zip_with_entry(
            "install_profile.json",
            vec![b' '; (MAX_INSTALLER_PROFILE_ENTRY_BYTES + 1) as usize],
        );

        let error = extract_installer(&jar).expect_err("oversized install profile should fail");

        assert!(
            matches!(error, ForgeInstallerError::EntryTooLarge { name } if name == "install_profile.json")
        );
    }

    #[test]
    fn extract_maven_entries_rejects_oversized_entry() {
        let root = test_root("oversized-maven-entry");
        let jar = zip_with_entry(
            "maven/example/mod.jar",
            vec![b'j'; (MAX_INSTALLER_EMBEDDED_ENTRY_BYTES + 1) as usize],
        );

        let error =
            extract_maven_entries(&jar, &root).expect_err("oversized maven entry should fail");

        assert!(
            matches!(error, ForgeInstallerError::EntryTooLarge { name } if name == "example/mod.jar")
        );
        assert!(
            !root
                .join("libraries")
                .join("example")
                .join("mod.jar")
                .exists()
        );
        let _ = fs::remove_dir_all(root);
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

    fn zip_contains(bytes: &[u8], name: &str) -> bool {
        let mut archive = zip::ZipArchive::new(Cursor::new(bytes)).expect("zip archive");
        archive.by_name(name).is_ok()
    }

    fn test_root(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|value| value.as_nanos())
            .unwrap_or_default();
        let root = std::env::temp_dir().join(format!(
            "axial-forge-installer-{name}-{}-{nanos:x}",
            std::process::id()
        ));
        fs::create_dir_all(&root).expect("create test root");
        root
    }
}
