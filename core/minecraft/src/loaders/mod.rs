mod forge_installer;
mod processors;

use crate::download::download_libraries;
use crate::download::{DownloadError, DownloadProgress, Downloader};
use crate::paths::versions_dir;
use crate::profiles::ensure_launcher_profiles;
use forge_installer::{extract_installer, extract_maven_entries};
use processors::run_processors;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs;
use std::path::Path;
use std::time::Duration;
use thiserror::Error;

const FABRIC_META_BASE: &str = "https://meta.fabricmc.net/v2/versions";
const QUILT_META_BASE: &str = "https://meta.quiltmc.org/v3/versions";
const FORGE_MAVEN_META: &str =
    "https://maven.minecraftforge.net/net/minecraftforge/forge/maven-metadata.xml";
const FORGE_PROMOTIONS_URL: &str =
    "https://files.minecraftforge.net/net/minecraftforge/forge/promotions_slim.json";
const NEOFORGE_MAVEN_META: &str =
    "https://maven.neoforged.net/releases/net/neoforged/neoforge/maven-metadata.xml";
const FORGE_MAVEN_BASE: &str = "https://maven.minecraftforge.net";
const NEOFORGE_MAVEN_BASE: &str = "https://maven.neoforged.net/releases";
const MAX_INSTALLER_DOWNLOAD_SIZE: u64 = 50 << 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoaderType {
    Fabric,
    Quilt,
    Forge,
    NeoForge,
}

impl LoaderType {
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "fabric" => Some(Self::Fabric),
            "quilt" => Some(Self::Quilt),
            "forge" => Some(Self::Forge),
            "neoforge" => Some(Self::NeoForge),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GameVersion {
    pub version: String,
    pub stable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoaderVersion {
    pub version: String,
    pub stable: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub recommended: bool,
}

#[derive(Debug, Error)]
pub enum LoaderError {
    #[error("invalid minecraft version")]
    InvalidMinecraftVersion,
    #[error("invalid loader version")]
    InvalidLoaderVersion,
    #[error("loader installs are not ported yet for this loader")]
    InstallNotImplemented,
    #[error("request failed: {0}")]
    Request(#[from] reqwest::Error),
    #[error("download failed: {0}")]
    Download(#[from] DownloadError),
    #[error("parse failed: {0}")]
    Parse(#[from] serde_json::Error),
    #[error("io failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Other(String),
}

pub async fn fetch_game_versions(loader_type: LoaderType) -> Result<Vec<GameVersion>, LoaderError> {
    let client = http_client()?;
    match loader_type {
        LoaderType::Fabric => {
            #[derive(Deserialize)]
            struct Entry {
                version: String,
                stable: bool,
            }
            let raw = client
                .get(format!("{FABRIC_META_BASE}/game"))
                .send()
                .await?
                .error_for_status()?
                .json::<Vec<Entry>>()
                .await?;
            Ok(raw
                .into_iter()
                .map(|entry| GameVersion {
                    version: entry.version,
                    stable: entry.stable,
                })
                .collect())
        }
        LoaderType::Quilt => {
            #[derive(Deserialize)]
            struct Entry {
                version: String,
                stable: bool,
            }
            let raw = client
                .get(format!("{QUILT_META_BASE}/game"))
                .send()
                .await?
                .error_for_status()?
                .json::<Vec<Entry>>()
                .await?;
            Ok(raw
                .into_iter()
                .map(|entry| GameVersion {
                    version: entry.version,
                    stable: entry.stable,
                })
                .collect())
        }
        LoaderType::Forge => {
            let xml = client
                .get(FORGE_MAVEN_META)
                .send()
                .await?
                .error_for_status()?
                .text()
                .await?;
            let entries = parse_maven_versions(&xml);
            let mut seen = HashSet::new();
            let mut versions = Vec::new();
            for entry in entries {
                let mc_version = extract_forge_mc_version(&entry);
                if mc_version.is_empty() || !seen.insert(mc_version.clone()) {
                    continue;
                }
                versions.push(GameVersion {
                    version: mc_version,
                    stable: true,
                });
            }
            Ok(versions)
        }
        LoaderType::NeoForge => {
            let xml = client
                .get(NEOFORGE_MAVEN_META)
                .send()
                .await?
                .error_for_status()?
                .text()
                .await?;
            let entries = parse_maven_versions(&xml);
            let mut seen = HashSet::new();
            let mut versions = Vec::new();
            for entry in entries {
                let mc_version = neoforge_to_mc_version(&entry);
                if mc_version.is_empty() || !seen.insert(mc_version.clone()) {
                    continue;
                }
                versions.push(GameVersion {
                    version: mc_version,
                    stable: true,
                });
            }
            Ok(versions)
        }
    }
}

pub async fn fetch_loader_versions(
    loader_type: LoaderType,
    mc_version: &str,
) -> Result<Vec<LoaderVersion>, LoaderError> {
    let mc_version = sanitize_segment(mc_version, LoaderError::InvalidMinecraftVersion)?;
    let client = http_client()?;
    match loader_type {
        LoaderType::Fabric => {
            #[derive(Deserialize)]
            struct Entry {
                loader: Loader,
            }
            #[derive(Deserialize)]
            struct Loader {
                version: String,
                stable: bool,
            }
            let raw = client
                .get(format!("{FABRIC_META_BASE}/loader/{mc_version}"))
                .send()
                .await?
                .error_for_status()?
                .json::<Vec<Entry>>()
                .await?;
            Ok(raw
                .into_iter()
                .map(|entry| LoaderVersion {
                    version: entry.loader.version,
                    stable: entry.loader.stable,
                    recommended: false,
                })
                .collect())
        }
        LoaderType::Quilt => {
            #[derive(Deserialize)]
            struct Entry {
                loader: Loader,
            }
            #[derive(Deserialize)]
            struct Loader {
                version: String,
            }
            let raw = client
                .get(format!("{QUILT_META_BASE}/loader/{mc_version}"))
                .send()
                .await?
                .error_for_status()?
                .json::<Vec<Entry>>()
                .await?;
            Ok(raw
                .into_iter()
                .map(|entry| LoaderVersion {
                    version: entry.loader.version,
                    stable: true,
                    recommended: false,
                })
                .collect())
        }
        LoaderType::Forge => {
            #[derive(Deserialize, Default)]
            struct Promotions {
                #[serde(default)]
                promos: std::collections::HashMap<String, String>,
            }

            let xml = client
                .get(FORGE_MAVEN_META)
                .send()
                .await?
                .error_for_status()?
                .text()
                .await?;
            let entries = parse_maven_versions(&xml);
            let promotions = client
                .get(FORGE_PROMOTIONS_URL)
                .send()
                .await?
                .error_for_status()?
                .json::<Promotions>()
                .await
                .unwrap_or_default();
            let recommended = promotions
                .promos
                .get(&format!("{mc_version}-recommended"))
                .cloned();
            let latest = promotions
                .promos
                .get(&format!("{mc_version}-latest"))
                .cloned();

            let mut versions = Vec::new();
            for entry in entries {
                if extract_forge_mc_version(&entry) != mc_version {
                    continue;
                }
                let loader_version = extract_forge_loader_version(&entry);
                if loader_version.is_empty() {
                    continue;
                }
                let is_recommended = recommended
                    .as_ref()
                    .is_some_and(|value| value == &loader_version)
                    || latest
                        .as_ref()
                        .is_some_and(|value| value == &loader_version);
                versions.push(LoaderVersion {
                    stable: recommended
                        .as_ref()
                        .is_some_and(|value| value == &loader_version),
                    recommended: is_recommended,
                    version: loader_version,
                });
            }
            versions.reverse();
            Ok(versions)
        }
        LoaderType::NeoForge => {
            let xml = client
                .get(NEOFORGE_MAVEN_META)
                .send()
                .await?
                .error_for_status()?
                .text()
                .await?;
            let entries = parse_maven_versions(&xml);
            let mut versions = Vec::new();
            for entry in entries {
                if neoforge_to_mc_version(&entry) != mc_version {
                    continue;
                }
                versions.push(LoaderVersion {
                    version: entry.clone(),
                    stable: !entry.contains("beta"),
                    recommended: false,
                });
            }
            versions.reverse();
            Ok(versions)
        }
    }
}

pub async fn install_loader<F>(
    mc_dir: &Path,
    loader_type: LoaderType,
    mc_version: &str,
    loader_version: &str,
    mut send: F,
) -> Result<String, LoaderError>
where
    F: FnMut(DownloadProgress),
{
    let mc_version = sanitize_segment(mc_version, LoaderError::InvalidMinecraftVersion)?;
    let loader_version = sanitize_segment(loader_version, LoaderError::InvalidLoaderVersion)?;

    match loader_type {
        LoaderType::Fabric | LoaderType::Quilt => {
            install_fabric_like(mc_dir, loader_type, &mc_version, &loader_version, &mut send).await
        }
        LoaderType::Forge => install_forge(mc_dir, &mc_version, &loader_version, &mut send).await,
        LoaderType::NeoForge => {
            install_neoforge(mc_dir, &mc_version, &loader_version, &mut send).await
        }
    }
}

async fn install_fabric_like<F>(
    mc_dir: &Path,
    loader_type: LoaderType,
    mc_version: &str,
    loader_version: &str,
    send: &mut F,
) -> Result<String, LoaderError>
where
    F: FnMut(DownloadProgress),
{
    let version_id = match loader_type {
        LoaderType::Fabric => format!("fabric-loader-{loader_version}-{mc_version}"),
        LoaderType::Quilt => format!("quilt-loader-{loader_version}-{mc_version}"),
        _ => return Err(LoaderError::InstallNotImplemented),
    };

    send(DownloadProgress {
        phase: "loader_meta".to_string(),
        current: 0,
        total: 1,
        file: Some(match loader_type {
            LoaderType::Fabric => "Fetching Fabric profile...".to_string(),
            LoaderType::Quilt => "Fetching Quilt profile...".to_string(),
            _ => String::new(),
        }),
        error: None,
        done: false,
    });

    let profile_url = match loader_type {
        LoaderType::Fabric => {
            format!("{FABRIC_META_BASE}/loader/{mc_version}/{loader_version}/profile/json")
        }
        LoaderType::Quilt => {
            format!("{QUILT_META_BASE}/loader/{mc_version}/{loader_version}/profile/json")
        }
        _ => unreachable!(),
    };

    let loader_downloader = Downloader::new(mc_dir.to_path_buf());
    loader_downloader
        .install_version(&version_id, Some(profile_url.as_str()), |progress| {
            if progress.done {
                return;
            }
            if let Some(mapped) = map_loader_progress(progress) {
                send(mapped);
            }
        })
        .await?;

    if !is_base_game_installed(mc_dir, mc_version) {
        let base_downloader = Downloader::new(mc_dir.to_path_buf());
        base_downloader
            .install_version(mc_version, None, |progress| {
                if !progress.done {
                    send(progress);
                }
            })
            .await?;
    }

    ensure_launcher_profiles(mc_dir, &version_id)?;
    send(DownloadProgress {
        phase: "done".to_string(),
        current: 1,
        total: 1,
        file: None,
        error: None,
        done: true,
    });

    Ok(version_id)
}

async fn install_forge<F>(
    mc_dir: &Path,
    mc_version: &str,
    loader_version: &str,
    send: &mut F,
) -> Result<String, LoaderError>
where
    F: FnMut(DownloadProgress),
{
    send(DownloadProgress {
        phase: "loader_meta".to_string(),
        current: 0,
        total: 1,
        file: Some("Downloading Forge installer...".to_string()),
        error: None,
        done: false,
    });

    let installer_coord = format!("{mc_version}-{loader_version}");
    let installer_url = format!(
        "{FORGE_MAVEN_BASE}/net/minecraftforge/forge/{0}/forge-{0}-installer.jar",
        installer_coord
    );
    let installer_data = download_to_memory(&installer_url).await?;

    send(DownloadProgress {
        phase: "loader_json".to_string(),
        current: 0,
        total: 1,
        file: Some("Extracting Forge installer...".to_string()),
        error: None,
        done: false,
    });

    let extracted = extract_installer(&installer_data)
        .map_err(|error| LoaderError::Other(format!("extracting Forge installer: {error}")))?;

    if !is_base_game_installed(mc_dir, mc_version) {
        let base_downloader = Downloader::new(mc_dir.to_path_buf());
        base_downloader
            .install_version(mc_version, None, |progress| {
                if !progress.done {
                    send(progress);
                }
            })
            .await?;
    }

    let version_dir = versions_dir(mc_dir).join(&extracted.version_id);
    fs::create_dir_all(&version_dir)?;
    let marker_path = version_dir.join(".incomplete");
    let json_path = version_dir.join(format!("{}.json", extracted.version_id));
    fs::write(&marker_path, b"installing")?;
    fs::write(&json_path, &extracted.version_json)?;

    if let Err(error) = extract_maven_entries(&installer_data, mc_dir) {
        return Err(LoaderError::Other(format!(
            "extracting Forge installer libraries: {error}"
        )));
    }

    if let Err(error) = download_libraries(
        mc_dir,
        &extracted.libraries,
        "loader_libraries",
        |progress| {
            send(progress);
        },
    )
    .await
    {
        return Err(LoaderError::Other(format!(
            "downloading Forge libraries: {error}"
        )));
    }

    if let Some(install_profile_json) = extracted.install_profile_json.as_deref() {
        send(DownloadProgress {
            phase: "loader_processors".to_string(),
            current: 0,
            total: 1,
            file: Some("Running processors...".to_string()),
            error: None,
            done: false,
        });
        if let Err(error) = run_processors(
            mc_dir,
            mc_version,
            install_profile_json,
            &installer_data,
            |current, total, detail| {
                send(DownloadProgress {
                    phase: "loader_processors".to_string(),
                    current: current as i32,
                    total: total as i32,
                    file: Some(detail),
                    error: None,
                    done: false,
                });
            },
        )
        .await
        {
            return Err(LoaderError::Other(format!(
                "running Forge processors: {error}"
            )));
        }
    }

    let _ = fs::remove_file(&marker_path);
    ensure_launcher_profiles(mc_dir, &extracted.version_id)?;
    send(DownloadProgress {
        phase: "done".to_string(),
        current: 1,
        total: 1,
        file: None,
        error: None,
        done: true,
    });

    Ok(extracted.version_id)
}

async fn install_neoforge<F>(
    mc_dir: &Path,
    mc_version: &str,
    loader_version: &str,
    send: &mut F,
) -> Result<String, LoaderError>
where
    F: FnMut(DownloadProgress),
{
    send(DownloadProgress {
        phase: "loader_meta".to_string(),
        current: 0,
        total: 1,
        file: Some("Downloading NeoForge installer...".to_string()),
        error: None,
        done: false,
    });

    let installer_url = format!(
        "{NEOFORGE_MAVEN_BASE}/net/neoforged/neoforge/{0}/neoforge-{0}-installer.jar",
        loader_version
    );
    let installer_data = download_to_memory(&installer_url).await?;

    send(DownloadProgress {
        phase: "loader_json".to_string(),
        current: 0,
        total: 1,
        file: Some("Extracting NeoForge installer...".to_string()),
        error: None,
        done: false,
    });

    let extracted = extract_installer(&installer_data)
        .map_err(|error| LoaderError::Other(format!("extracting NeoForge installer: {error}")))?;

    if !is_base_game_installed(mc_dir, mc_version) {
        let base_downloader = Downloader::new(mc_dir.to_path_buf());
        base_downloader
            .install_version(mc_version, None, |progress| {
                if !progress.done {
                    send(progress);
                }
            })
            .await?;
    }

    let version_dir = versions_dir(mc_dir).join(&extracted.version_id);
    fs::create_dir_all(&version_dir)?;
    let marker_path = version_dir.join(".incomplete");
    let json_path = version_dir.join(format!("{}.json", extracted.version_id));
    fs::write(&marker_path, b"installing")?;
    fs::write(&json_path, &extracted.version_json)?;

    if let Err(error) = extract_maven_entries(&installer_data, mc_dir) {
        return Err(LoaderError::Other(format!(
            "extracting NeoForge installer libraries: {error}"
        )));
    }

    if let Err(error) = download_libraries(
        mc_dir,
        &extracted.libraries,
        "loader_libraries",
        |progress| {
            send(progress);
        },
    )
    .await
    {
        return Err(LoaderError::Other(format!(
            "downloading NeoForge libraries: {error}"
        )));
    }

    if let Some(install_profile_json) = extracted.install_profile_json.as_deref() {
        send(DownloadProgress {
            phase: "loader_processors".to_string(),
            current: 0,
            total: 1,
            file: Some("Running processors...".to_string()),
            error: None,
            done: false,
        });
        if let Err(error) = run_processors(
            mc_dir,
            mc_version,
            install_profile_json,
            &installer_data,
            |current, total, detail| {
                send(DownloadProgress {
                    phase: "loader_processors".to_string(),
                    current: current as i32,
                    total: total as i32,
                    file: Some(detail),
                    error: None,
                    done: false,
                });
            },
        )
        .await
        {
            return Err(LoaderError::Other(format!(
                "running NeoForge processors: {error}"
            )));
        }
    }

    let _ = fs::remove_file(&marker_path);
    ensure_launcher_profiles(mc_dir, &extracted.version_id)?;
    send(DownloadProgress {
        phase: "done".to_string(),
        current: 1,
        total: 1,
        file: None,
        error: None,
        done: true,
    });

    Ok(extracted.version_id)
}

fn map_loader_progress(progress: DownloadProgress) -> Option<DownloadProgress> {
    let phase = match progress.phase.as_str() {
        "version_json" => "loader_json",
        "libraries" => "loader_libraries",
        "error" => "error",
        _ => return None,
    };

    Some(DownloadProgress {
        phase: phase.to_string(),
        current: progress.current,
        total: progress.total,
        file: progress.file,
        error: progress.error,
        done: progress.done,
    })
}

fn is_base_game_installed(mc_dir: &Path, game_version: &str) -> bool {
    let version_dir = versions_dir(mc_dir).join(game_version);
    let json_path = version_dir.join(format!("{game_version}.json"));
    let jar_path = version_dir.join(format!("{game_version}.jar"));
    let marker_path = version_dir.join(".incomplete");
    json_path.is_file() && jar_path.is_file() && !marker_path.exists()
}

fn sanitize_segment(value: &str, invalid: LoaderError) -> Result<String, LoaderError> {
    let value = value.trim();
    if value.is_empty()
        || value.contains("..")
        || !value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-'))
    {
        return Err(invalid);
    }
    Ok(value.to_string())
}

fn http_client() -> Result<reqwest::Client, LoaderError> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(120))
        .build()
        .map_err(LoaderError::Request)
}

async fn download_to_memory(url: &str) -> Result<Vec<u8>, LoaderError> {
    let client = http_client()?;
    let response = client.get(url).send().await?.error_for_status()?;
    if response
        .content_length()
        .is_some_and(|length| length > MAX_INSTALLER_DOWNLOAD_SIZE)
    {
        return Err(LoaderError::Other(format!("download too large for {url}")));
    }
    let bytes = response.bytes().await?;
    if bytes.len() as u64 > MAX_INSTALLER_DOWNLOAD_SIZE {
        return Err(LoaderError::Other(format!("download too large for {url}")));
    }
    Ok(bytes.to_vec())
}

fn parse_maven_versions(xml: &str) -> Vec<String> {
    let pattern = Regex::new(r"<version>([^<]+)</version>").expect("valid regex");
    pattern
        .captures_iter(xml)
        .filter_map(|capture| capture.get(1).map(|value| value.as_str().to_string()))
        .collect()
}

fn extract_forge_mc_version(entry: &str) -> String {
    entry
        .split_once('-')
        .map(|(mc_version, _)| mc_version.to_string())
        .unwrap_or_default()
}

fn extract_forge_loader_version(entry: &str) -> String {
    entry
        .split_once('-')
        .map(|(_, loader_version)| loader_version.to_string())
        .unwrap_or_default()
}

fn neoforge_to_mc_version(version: &str) -> String {
    let mut parts = version.splitn(3, '.');
    let Some(major) = parts.next() else {
        return String::new();
    };
    let Some(minor) = parts.next() else {
        return String::new();
    };
    if minor == "0" {
        format!("1.{major}")
    } else {
        format!("1.{major}.{minor}")
    }
}

#[cfg(test)]
mod tests {
    use super::{
        extract_forge_loader_version, extract_forge_mc_version, neoforge_to_mc_version,
        parse_maven_versions,
    };

    #[test]
    fn parses_maven_versions() {
        let xml = "<metadata><versioning><versions><version>1.20.1-47.3.0</version><version>1.20.4-49.0.2</version></versions></versioning></metadata>";
        let versions = parse_maven_versions(xml);
        assert_eq!(versions, vec!["1.20.1-47.3.0", "1.20.4-49.0.2"]);
    }

    #[test]
    fn splits_forge_coordinates() {
        assert_eq!(extract_forge_mc_version("1.20.1-47.3.0"), "1.20.1");
        assert_eq!(extract_forge_loader_version("1.20.1-47.3.0"), "47.3.0");
    }

    #[test]
    fn maps_neoforge_version_to_minecraft() {
        assert_eq!(neoforge_to_mc_version("20.4.237"), "1.20.4");
        assert_eq!(neoforge_to_mc_version("21.0.1"), "1.21");
        assert_eq!(neoforge_to_mc_version("21.4.1"), "1.21.4");
    }
}
