use super::compose::LoaderProfileFragment;
use crate::download::DownloadError;
use crate::launch::Library;
use crate::paths::libraries_dir;
use serde::Deserialize;
use std::collections::HashSet;
use std::fs;
use std::io::Read;
use std::path::Path;
use thiserror::Error;
use zip::ZipArchive;

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
        let mut output = fs::File::create(&destination)?;
        let mut bounded = (&mut file).take(MAX_INSTALLER_EMBEDDED_ENTRY_BYTES + 1);
        let copied = std::io::copy(&mut bounded, &mut output)?;
        if copied > MAX_INSTALLER_EMBEDDED_ENTRY_BYTES {
            let _ = fs::remove_file(&destination);
            return Err(ForgeInstallerError::EntryTooLarge {
                name: relative.to_string(),
            });
        }
    }

    Ok(())
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
    let mut version_info = profile.version_info;

    if let Some(version_id) =
        normalize_legacy_forge_version_id(&profile.install.path, &profile.minecraft).or_else(|| {
            (!profile.install.target.is_empty()).then(|| profile.install.target.clone())
        })
    {
        version_info["id"] = serde_json::Value::String(version_id);
    }

    if let Some(normalized_library) = normalize_legacy_forge_library(
        &profile.install.path,
        &profile.install.file_path,
        &profile.minecraft,
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
        let mut cursor = Cursor::new(Vec::new());
        {
            let mut writer = zip::ZipWriter::new(&mut cursor);
            writer
                .start_file(name, SimpleFileOptions::default())
                .expect("start zip file");
            writer.write_all(&bytes).expect("write zip file");
            writer.finish().expect("finish zip");
        }
        cursor.into_inner()
    }

    fn test_root(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|value| value.as_nanos())
            .unwrap_or_default();
        let root = std::env::temp_dir().join(format!(
            "croopor-forge-installer-{name}-{}-{nanos:x}",
            std::process::id()
        ));
        fs::create_dir_all(&root).expect("create test root");
        root
    }
}
