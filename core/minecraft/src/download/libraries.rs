use super::client::{library_download_concurrency, standard_minecraft_download_client};
use super::integrity::is_sha1_hex;
use super::model::{
    DownloadError, DownloadProgress, ExecutionDownloadFact, ExpectedIntegrity,
    SelectedDownloadArtifactDescriptor, SelectedDownloadArtifactKind, progress,
};
use super::path_safety::resolve_path_under_root;
use super::transfer::{
    ensure_selected_artifact_with_client,
    ensure_selected_artifact_with_client_allowing_missing_checksum,
};
use crate::launch::{Library, maven_to_path};
use crate::paths::libraries_dir;
use crate::rules::{current_os_arch, default_environment, evaluate_rules};
use futures_util::StreamExt;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use tokio::sync::mpsc;

#[derive(Debug, Clone)]
pub struct DownloadJob {
    pub path: PathBuf,
    pub url: String,
    pub name: String,
    pub expected: ExpectedIntegrity,
    pub allow_missing_checksum: bool,
}

pub async fn download_libraries<F>(
    mc_dir: &Path,
    libraries: &[Library],
    phase: &str,
    send: F,
) -> Result<(), DownloadError>
where
    F: FnMut(DownloadProgress),
{
    let env = default_environment();
    let jobs = library_jobs_for(mc_dir, libraries, &env);
    download_library_jobs(jobs, phase, send, None, None, false).await
}

pub async fn download_libraries_with_facts_and_descriptors<F, G, H>(
    mc_dir: &Path,
    libraries: &[Library],
    phase: &str,
    send: F,
    mut send_fact: G,
    mut send_descriptor: H,
) -> Result<(), DownloadError>
where
    F: FnMut(DownloadProgress),
    G: FnMut(ExecutionDownloadFact),
    H: FnMut(SelectedDownloadArtifactDescriptor),
{
    let env = default_environment();
    let jobs = library_jobs_for(mc_dir, libraries, &env);
    let (fact_tx, mut fact_rx) = mpsc::unbounded_channel();
    let (descriptor_tx, mut descriptor_rx) = mpsc::unbounded_channel();
    let result =
        download_library_jobs(jobs, phase, send, Some(fact_tx), Some(descriptor_tx), false).await;
    while let Ok(fact) = fact_rx.try_recv() {
        send_fact(fact);
    }
    while let Ok(descriptor) = descriptor_rx.try_recv() {
        send_descriptor(descriptor);
    }
    result
}

pub async fn download_libraries_allowing_missing_checksums_with_facts_and_descriptors<F, G, H>(
    mc_dir: &Path,
    libraries: &[Library],
    phase: &str,
    send: F,
    mut send_fact: G,
    mut send_descriptor: H,
) -> Result<(), DownloadError>
where
    F: FnMut(DownloadProgress),
    G: FnMut(ExecutionDownloadFact),
    H: FnMut(SelectedDownloadArtifactDescriptor),
{
    let env = default_environment();
    let jobs = library_jobs_for(mc_dir, libraries, &env);
    let (fact_tx, mut fact_rx) = mpsc::unbounded_channel();
    let (descriptor_tx, mut descriptor_rx) = mpsc::unbounded_channel();
    let result =
        download_library_jobs(jobs, phase, send, Some(fact_tx), Some(descriptor_tx), true).await;
    while let Ok(fact) = fact_rx.try_recv() {
        send_fact(fact);
    }
    while let Ok(descriptor) = descriptor_rx.try_recv() {
        send_descriptor(descriptor);
    }
    result
}

async fn download_library_jobs<F>(
    jobs: Vec<DownloadJob>,
    phase: &str,
    mut send: F,
    fact_tx: Option<mpsc::UnboundedSender<ExecutionDownloadFact>>,
    descriptor_tx: Option<mpsc::UnboundedSender<SelectedDownloadArtifactDescriptor>>,
    allow_missing_checksum: bool,
) -> Result<(), DownloadError>
where
    F: FnMut(DownloadProgress),
{
    let client = standard_minecraft_download_client();
    send(progress(phase, 0, jobs.len() as i32, None));
    let total_jobs = jobs.len() as i32;
    let mut completed_jobs = 0;
    let mut downloads = futures_util::stream::iter(jobs.into_iter().map(|job| {
        let client = client.clone();
        let fact_tx = fact_tx.clone();
        let descriptor_tx = descriptor_tx.clone();
        async move {
            if allow_missing_checksum {
                ensure_selected_artifact_with_client_allowing_missing_checksum(
                    SelectedDownloadArtifactKind::Library,
                    &client,
                    &job.url,
                    &job.path,
                    &job.expected,
                    fact_tx.as_ref(),
                    descriptor_tx.as_ref(),
                )
                .await?;
            } else {
                ensure_selected_artifact_with_client(
                    SelectedDownloadArtifactKind::Library,
                    &client,
                    &job.url,
                    &job.path,
                    &job.expected,
                    fact_tx.as_ref(),
                    descriptor_tx.as_ref(),
                )
                .await?;
            }
            Ok::<String, DownloadError>(job.name)
        }
    }))
    .buffer_unordered(library_download_concurrency());
    while let Some(result) = downloads.next().await {
        let name = result?;
        completed_jobs += 1;
        send(progress(phase, completed_jobs, total_jobs, Some(name)));
    }
    Ok(())
}

pub fn resolve_library_download(lib: &Library, mc_dir: &Path) -> Option<DownloadJob> {
    let lib_dir = libraries_dir(mc_dir);
    if !lib.natives.is_empty()
        && lib
            .downloads
            .as_ref()
            .is_none_or(|downloads| downloads.artifact.is_none())
    {
        return None;
    }

    if let Some(artifact) = lib
        .downloads
        .as_ref()
        .and_then(|downloads| downloads.artifact.as_ref())
    {
        if !artifact.url.trim().is_empty() {
            let path = resolve_path_under_root(&lib_dir, &artifact.path)?;
            return Some(DownloadJob {
                name: Path::new(&artifact.path)
                    .file_name()
                    .map(|value| value.to_string_lossy().to_string())
                    .unwrap_or_else(|| lib.name.clone()),
                path,
                url: artifact.url.clone(),
                expected: library_expected_integrity(lib, artifact.size, &artifact.sha1),
                allow_missing_checksum: lib.croopor_checksumless_allowed,
            });
        }
        return None;
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
        expected: library_expected_integrity(lib, lib.size, &lib.sha1),
        allow_missing_checksum: lib.croopor_checksumless_allowed,
    })
}

pub fn resolve_native_download(lib: &Library, mc_dir: &Path, os_name: &str) -> Option<DownloadJob> {
    let lib_dir = libraries_dir(mc_dir);
    for classifier_key in native_classifier_candidates(lib, os_name) {
        if let Some(artifact) = lib
            .downloads
            .as_ref()
            .and_then(|downloads| downloads.classifiers.get(&classifier_key))
            && !artifact.url.trim().is_empty()
        {
            let path = resolve_path_under_root(&lib_dir, &artifact.path)?;
            return Some(DownloadJob {
                name: Path::new(&artifact.path)
                    .file_name()
                    .map(|value| value.to_string_lossy().to_string())
                    .unwrap_or_else(|| format!("{}:{classifier_key}", lib.name)),
                path,
                url: artifact.url.clone(),
                expected: library_expected_integrity(lib, artifact.size, &artifact.sha1),
                allow_missing_checksum: lib.croopor_checksumless_allowed,
            });
        }
    }

    let classifier_key = native_classifier_candidates(lib, os_name)
        .into_iter()
        .next()?;
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
        expected: library_expected_integrity(lib, lib.size, &lib.sha1),
        allow_missing_checksum: lib.croopor_checksumless_allowed,
    })
}

fn library_expected_integrity(lib: &Library, size: i64, sha1: &str) -> ExpectedIntegrity {
    let expected = ExpectedIntegrity::from_mojang(size, sha1);
    if expected.sha1.is_some() {
        return expected;
    }
    lib.checksums
        .iter()
        .map(|checksum| checksum.trim())
        .find(|checksum| is_sha1_hex(checksum))
        .map(ExpectedIntegrity::from_sha1)
        .unwrap_or(expected)
}

pub fn native_classifier_candidates(lib: &Library, os_name: &str) -> Vec<String> {
    let Some(base) = lib.natives.get(os_name) else {
        return Vec::new();
    };

    let arch = current_os_arch();
    let mut candidates = Vec::new();
    let variants = match arch {
        "x86_64" => vec![
            base.replace("${arch}", "64"),
            base.replace("-${arch}", ""),
            base.replace("${arch}", "x86_64"),
        ],
        "x86" => vec![
            base.replace("${arch}", "32"),
            base.replace("${arch}", "x86"),
        ],
        "arm64" => vec![
            base.replace("${arch}", "arm64"),
            base.replace("${arch}", "64"),
        ],
        _ => vec![base.replace("${arch}", arch)],
    };

    for variant in variants {
        if !variant.is_empty() && !candidates.contains(&variant) {
            candidates.push(variant);
        }
    }

    candidates
}

pub fn library_jobs_for(
    mc_dir: &Path,
    libraries: &[Library],
    env: &crate::rules::Environment,
) -> Vec<DownloadJob> {
    let mut jobs = Vec::new();
    let mut queued_paths = HashSet::new();

    for lib in libraries {
        if !evaluate_rules(&lib.rules, env) {
            continue;
        }

        if crate::rules::is_native_library(&lib.name) && !native_name_matches_env(&lib.name, env) {
            continue;
        }

        if let Some(job) = resolve_library_download(lib, mc_dir)
            && queued_paths.insert(job.path.clone())
        {
            jobs.push(job);
        }
        if let Some(job) = resolve_native_download(lib, mc_dir, &env.os_name)
            && queued_paths.insert(job.path.clone())
        {
            jobs.push(job);
        }
    }

    jobs
}

fn native_name_matches_env(name: &str, env: &crate::rules::Environment) -> bool {
    let lower = name.to_ascii_lowercase();
    if !lower.contains("natives-") {
        return true;
    }
    if lower.contains("windows-arm64") {
        return env.os_name == "windows" && env.os_arch == "arm64";
    }
    if lower.contains("windows-x86") {
        return env.os_name == "windows" && env.os_arch == "x86";
    }
    if lower.contains("natives-windows") {
        return env.os_name == "windows" && env.os_arch == "x86_64";
    }
    if lower.contains("macos-arm64") || lower.contains("osx-arm64") {
        return env.os_name == "osx" && env.os_arch == "arm64";
    }
    if lower.contains("natives-macos") || lower.contains("natives-osx") {
        return env.os_name == "osx" && env.os_arch == "x86_64";
    }
    if lower.contains("linux-arm64") {
        return env.os_name == "linux" && env.os_arch == "arm64";
    }
    if lower.contains("linux-x86") {
        return env.os_name == "linux" && env.os_arch == "x86";
    }
    if lower.contains("natives-linux") {
        return env.os_name == "linux" && env.os_arch == "x86_64";
    }
    true
}
