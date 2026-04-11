use crate::launch::{Library, VersionJson, maven_to_path};
use crate::manifest::fetch_version_manifest;
use crate::paths::{assets_dir, libraries_dir, versions_dir};
use crate::rules::{default_environment, evaluate_rules};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DownloadProgress {
    pub phase: String,
    pub current: i32,
    pub total: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub done: bool,
}

pub struct Downloader {
    mc_dir: PathBuf,
    client: reqwest::Client,
}

#[derive(Debug, Error)]
pub enum DownloadError {
    #[error("create directory: {0}")]
    CreateDirectory(#[from] io::Error),
    #[error("resolve manifest url: {0}")]
    ResolveManifest(String),
    #[error("request failed: {0}")]
    Request(#[from] reqwest::Error),
    #[error("parse version json: {0}")]
    ParseVersion(#[from] serde_json::Error),
}

#[derive(Debug, Clone)]
struct DownloadJob {
    path: PathBuf,
    url: String,
    name: String,
}

impl Downloader {
    pub fn new(mc_dir: impl Into<PathBuf>) -> Self {
        Self {
            mc_dir: mc_dir.into(),
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(300))
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
        }
    }

    pub async fn install_version<F>(
        &self,
        version_id: &str,
        manifest_url: Option<&str>,
        mut send: F,
    ) -> Result<(), DownloadError>
    where
        F: FnMut(DownloadProgress),
    {
        let version_dir = versions_dir(&self.mc_dir).join(version_id);
        fs::create_dir_all(&version_dir)?;

        let marker_path = version_dir.join(".incomplete");
        fs::write(&marker_path, b"installing")?;

        let install_result = self
            .install_version_inner(version_id, manifest_url, &mut send)
            .await;

        match install_result {
            Ok(()) => {
                let _ = fs::remove_file(&marker_path);
                send(DownloadProgress {
                    phase: "done".to_string(),
                    current: 1,
                    total: 1,
                    file: None,
                    error: None,
                    done: true,
                });
                Ok(())
            }
            Err(error) => {
                send(DownloadProgress {
                    phase: "error".to_string(),
                    current: 0,
                    total: 0,
                    file: None,
                    error: Some(error.to_string()),
                    done: true,
                });
                Err(error)
            }
        }
    }

    async fn install_version_inner<F>(
        &self,
        version_id: &str,
        manifest_url: Option<&str>,
        send: &mut F,
    ) -> Result<(), DownloadError>
    where
        F: FnMut(DownloadProgress),
    {
        let version_dir = versions_dir(&self.mc_dir).join(version_id);
        let json_path = version_dir.join(format!("{version_id}.json"));
        send(progress(
            "version_json",
            0,
            1,
            Some(format!("{version_id}.json")),
        ));

        let url = if let Some(url) = manifest_url.filter(|value| !value.trim().is_empty()) {
            url.to_string()
        } else if json_path.is_file() {
            String::new()
        } else {
            self.resolve_manifest_url(version_id).await?
        };
        if !url.is_empty() {
            self.download_file(&url, &json_path).await?;
        }

        let version = serde_json::from_str::<VersionJson>(&fs::read_to_string(&json_path)?)?;

        send(progress(
            "client_jar",
            0,
            1,
            Some(format!("{version_id}.jar")),
        ));
        if let Some(client) = &version.downloads.client {
            let jar_path = version_dir.join(format!("{version_id}.jar"));
            if !jar_path.is_file() {
                self.download_file(&client.url, &jar_path).await?;
            }
        }

        let library_jobs = self.library_jobs(&version);
        send(progress("libraries", 0, library_jobs.len() as i32, None));
        for (index, job) in library_jobs.iter().enumerate() {
            if !job.path.is_file() {
                self.download_file(&job.url, &job.path).await?;
            }
            send(progress(
                "libraries",
                (index + 1) as i32,
                library_jobs.len() as i32,
                Some(job.name.clone()),
            ));
        }

        if !version.asset_index.url.is_empty() {
            let asset_index_path = assets_dir(&self.mc_dir)
                .join("indexes")
                .join(format!("{}.json", version.asset_index.id));
            send(progress(
                "asset_index",
                0,
                1,
                Some(format!("{}.json", version.asset_index.id)),
            ));
            if !asset_index_path.is_file() {
                self.download_file(&version.asset_index.url, &asset_index_path)
                    .await?;
            }
            self.download_asset_objects(&asset_index_path, send).await?;
        }

        if let Some(logging) = version
            .logging
            .as_ref()
            .and_then(|logging| logging.client.as_ref())
            && !logging.file.url.is_empty()
        {
            let log_config_path = assets_dir(&self.mc_dir)
                .join("log_configs")
                .join(&logging.file.id);
            send(progress("log_config", 0, 1, Some(logging.file.id.clone())));
            if !log_config_path.is_file() {
                self.download_file(&logging.file.url, &log_config_path)
                    .await?;
            }
        }

        Ok(())
    }

    async fn resolve_manifest_url(&self, version_id: &str) -> Result<String, DownloadError> {
        let manifest = fetch_version_manifest()
            .await
            .map_err(|error| DownloadError::ResolveManifest(error.to_string()))?;
        manifest
            .versions
            .into_iter()
            .find(|entry| entry.id == version_id)
            .map(|entry| entry.url)
            .ok_or_else(|| {
                DownloadError::ResolveManifest(format!(
                    "version {version_id} not found in manifest"
                ))
            })
    }

    fn library_jobs(&self, version: &VersionJson) -> Vec<DownloadJob> {
        let env = default_environment();
        library_jobs_for(&self.mc_dir, &version.libraries, &env)
    }

    async fn download_asset_objects<F>(
        &self,
        asset_index_path: &Path,
        send: &mut F,
    ) -> Result<(), DownloadError>
    where
        F: FnMut(DownloadProgress),
    {
        #[derive(Deserialize)]
        struct AssetIndex {
            objects: std::collections::HashMap<String, AssetObject>,
            #[serde(default, rename = "virtual")]
            virtual_flag: bool,
            #[serde(default, rename = "map_to_resources")]
            map_to_resources: bool,
        }

        #[derive(Deserialize)]
        struct AssetObject {
            hash: String,
        }

        let index = serde_json::from_str::<AssetIndex>(&fs::read_to_string(asset_index_path)?)?;
        let objects_dir = assets_dir(&self.mc_dir).join("objects");
        let mut jobs = Vec::new();
        for object in index.objects.values() {
            let prefix = &object.hash[..2];
            let path = objects_dir.join(prefix).join(&object.hash);
            if !path.is_file() {
                jobs.push((object.hash.clone(), path));
            }
        }

        send(progress("assets", 0, jobs.len() as i32, None));
        for (index_value, (hash, path)) in jobs.iter().enumerate() {
            let url = format!(
                "https://resources.download.minecraft.net/{}/{}",
                &hash[..2],
                hash
            );
            self.download_file(&url, path).await?;
            if index_value + 1 == jobs.len() || (index_value + 1) % 50 == 0 {
                send(progress(
                    "assets",
                    (index_value + 1) as i32,
                    jobs.len() as i32,
                    None,
                ));
            }
        }

        if index.virtual_flag || index.map_to_resources {
            let virtual_dir = assets_dir(&self.mc_dir).join("virtual").join("legacy");
            for (name, object) in index.objects {
                let src = objects_dir.join(&object.hash[..2]).join(&object.hash);
                let dst = virtual_dir.join(PathBuf::from(name));
                if dst.is_file() || !src.is_file() {
                    continue;
                }
                if let Some(parent) = dst.parent() {
                    fs::create_dir_all(parent)?;
                }
                let _ = fs::copy(src, dst);
            }
        }

        Ok(())
    }

    async fn download_file(&self, url: &str, destination: &Path) -> Result<(), DownloadError> {
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)?;
        }

        let response = self.client.get(url).send().await?.error_for_status()?;
        let tmp_path = destination.with_extension("tmp");
        let mut output = fs::File::create(&tmp_path)?;
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            output.write_all(&chunk)?;
        }
        output.flush()?;
        fs::rename(tmp_path, destination)?;
        Ok(())
    }
}

pub async fn download_libraries<F>(
    mc_dir: &Path,
    libraries: &[Library],
    phase: &str,
    mut send: F,
) -> Result<(), DownloadError>
where
    F: FnMut(DownloadProgress),
{
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(300))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());
    let env = default_environment();
    let jobs = library_jobs_for(mc_dir, libraries, &env);

    send(progress(phase, 0, jobs.len() as i32, None));
    for (index, job) in jobs.iter().enumerate() {
        if !job.path.is_file() {
            download_file_with_client(&client, &job.url, &job.path).await?;
        }
        send(progress(
            phase,
            (index + 1) as i32,
            jobs.len() as i32,
            Some(job.name.clone()),
        ));
    }
    Ok(())
}

fn progress(phase: &str, current: i32, total: i32, file: Option<String>) -> DownloadProgress {
    DownloadProgress {
        phase: phase.to_string(),
        current,
        total,
        file,
        error: None,
        done: false,
    }
}

fn resolve_library_download(lib: &Library, mc_dir: &Path) -> Option<DownloadJob> {
    let lib_dir = libraries_dir(mc_dir);
    if !lib.natives.is_empty()
        && lib
            .downloads
            .as_ref()
            .is_some_and(|downloads| downloads.artifact.is_none())
    {
        return None;
    }

    if let Some(artifact) = lib
        .downloads
        .as_ref()
        .and_then(|downloads| downloads.artifact.as_ref())
    {
        let path = resolve_path_under_root(&lib_dir, &artifact.path)?;
        return Some(DownloadJob {
            name: Path::new(&artifact.path)
                .file_name()
                .map(|value| value.to_string_lossy().to_string())
                .unwrap_or_else(|| lib.name.clone()),
            path,
            url: artifact.url.clone(),
        });
    }

    let maven_path = maven_to_path(&lib.name);
    if maven_path.as_os_str().is_empty() {
        return None;
    }
    let base_url = if lib.url.is_empty() {
        "https://libraries.minecraft.net/".to_string()
    } else if lib.url.ends_with('/') {
        lib.url.clone()
    } else {
        format!("{}/", lib.url)
    };
    let path = lib_dir.join(&maven_path);
    Some(DownloadJob {
        name: path
            .file_name()
            .map(|value| value.to_string_lossy().to_string())
            .unwrap_or_else(|| lib.name.clone()),
        path,
        url: format!(
            "{}{}",
            base_url,
            maven_path.to_string_lossy().replace('\\', "/")
        ),
    })
}

fn resolve_native_download(lib: &Library, mc_dir: &Path, os_name: &str) -> Option<DownloadJob> {
    let classifier_key = lib.natives.get(os_name)?.replace(
        "${arch}",
        if cfg!(target_arch = "x86_64") || cfg!(target_arch = "aarch64") {
            "64"
        } else {
            "32"
        },
    );

    let lib_dir = libraries_dir(mc_dir);
    if let Some(artifact) = lib
        .downloads
        .as_ref()
        .and_then(|downloads| downloads.classifiers.get(&classifier_key))
    {
        let path = resolve_path_under_root(&lib_dir, &artifact.path)?;
        return Some(DownloadJob {
            name: Path::new(&artifact.path)
                .file_name()
                .map(|value| value.to_string_lossy().to_string())
                .unwrap_or_else(|| format!("{}:{classifier_key}", lib.name)),
            path,
            url: artifact.url.clone(),
        });
    }

    let maven_path = maven_to_path(&format!("{}:{classifier_key}", lib.name));
    if maven_path.as_os_str().is_empty() {
        return None;
    }
    let base_url = if lib.url.is_empty() {
        "https://libraries.minecraft.net/".to_string()
    } else if lib.url.ends_with('/') {
        lib.url.clone()
    } else {
        format!("{}/", lib.url)
    };
    let path = lib_dir.join(&maven_path);
    Some(DownloadJob {
        name: path
            .file_name()
            .map(|value| value.to_string_lossy().to_string())
            .unwrap_or_else(|| format!("{}:{classifier_key}", lib.name)),
        path,
        url: format!(
            "{}{}",
            base_url,
            maven_path.to_string_lossy().replace('\\', "/")
        ),
    })
}

fn library_jobs_for(
    mc_dir: &Path,
    libraries: &[Library],
    env: &crate::rules::Environment,
) -> Vec<DownloadJob> {
    let mut jobs = Vec::new();

    for lib in libraries {
        if !evaluate_rules(&lib.rules, env) {
            continue;
        }

        if let Some(job) = resolve_library_download(lib, mc_dir) {
            jobs.push(job);
        }
        if let Some(job) = resolve_native_download(lib, mc_dir, &env.os_name) {
            jobs.push(job);
        }
    }

    jobs
}

async fn download_file_with_client(
    client: &reqwest::Client,
    url: &str,
    destination: &Path,
) -> Result<(), DownloadError> {
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
    }

    let response = client.get(url).send().await?.error_for_status()?;
    let tmp_path = destination.with_extension("tmp");
    let mut output = fs::File::create(&tmp_path)?;
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        output.write_all(&chunk)?;
    }
    output.flush()?;
    fs::rename(tmp_path, destination)?;
    Ok(())
}

fn resolve_path_under_root(root: &Path, relative: &str) -> Option<PathBuf> {
    let clean = PathBuf::from(relative.replace('/', std::path::MAIN_SEPARATOR_STR));
    if clean.as_os_str().is_empty() || clean.is_absolute() {
        return None;
    }
    let joined = root.join(&clean);
    let relative_check = joined.strip_prefix(root).ok()?;
    if relative_check
        .components()
        .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        return None;
    }
    Some(joined)
}
