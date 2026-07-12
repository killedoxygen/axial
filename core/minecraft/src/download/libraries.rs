use super::client::{library_download_concurrency, standard_minecraft_download_client};
use super::integrity::is_sha1_hex;
use super::model::{
    DownloadError, DownloadProgress, ExecutionDownloadFact, ExpectedIntegrity, LibraryPlanError,
    SelectedDownloadArtifactDescriptor, SelectedDownloadArtifactKind, progress,
};
use super::transfer::{
    ensure_selected_artifact_with_client,
    ensure_selected_artifact_with_client_allowing_missing_checksum,
};
use crate::artifact_path::ArtifactRelativePath;
use crate::launch::{Library, maven_to_path};
use crate::paths::libraries_dir;
use crate::rules::{Environment, default_environment, evaluate_rules};
use futures_util::StreamExt;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use tokio::sync::mpsc;

#[derive(Debug, Clone)]
pub(crate) struct DownloadJob {
    pub(crate) path: PathBuf,
    pub(crate) url: String,
    pub(crate) name: String,
    pub(crate) expected: ExpectedIntegrity,
    pub(crate) allow_missing_checksum: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LibraryVerificationPlan {
    pub path: PathBuf,
    pub name: String,
    pub integrity: LibraryVerificationIntegrity,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StructuralLibraryVerification {
    pub(crate) minecraft_root: PathBuf,
    pub(crate) relative_path: ArtifactRelativePath,
    pub(crate) expected_size: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LibraryVerificationIntegrity {
    Sha1(ExpectedIntegrity),
    StructuralJar(StructuralLibraryVerification),
    MissingChecksum,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LibraryChecksumPolicy {
    Strict,
    AllowMissing,
}

impl LibraryChecksumPolicy {
    fn allows_missing(self) -> bool {
        matches!(self, Self::AllowMissing)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LibraryArtifactPlan {
    pub(crate) relative_path: ArtifactRelativePath,
    pub(crate) source_url: Option<String>,
    pub(crate) name: String,
    pub(crate) expected: ExpectedIntegrity,
    pub(crate) allow_missing_checksum: bool,
    pub(crate) is_native: bool,
}

impl LibraryArtifactPlan {
    fn into_verification_plan(
        self,
        mc_dir: &Path,
        known_good: Option<&crate::known_good::KnownGoodInventoryAuthority>,
    ) -> LibraryVerificationPlan {
        let integrity = if self.expected.sha1.is_some() {
            LibraryVerificationIntegrity::Sha1(self.expected.clone())
        } else if let Some(managed_root) = known_good.and_then(|inventory| {
            inventory.authorizes_structural_library(
                mc_dir,
                &self.relative_path,
                self.is_native,
                self.expected.size,
            )
        }) {
            LibraryVerificationIntegrity::StructuralJar(StructuralLibraryVerification {
                minecraft_root: managed_root,
                relative_path: self.relative_path.clone(),
                expected_size: self.expected.size,
            })
        } else {
            LibraryVerificationIntegrity::MissingChecksum
        };
        LibraryVerificationPlan {
            path: self.relative_path.join_under(&libraries_dir(mc_dir)),
            name: self.name,
            integrity,
        }
    }

    fn into_download_job(self, mc_dir: &Path) -> Result<DownloadJob, LibraryPlanError> {
        let url = self
            .source_url
            .ok_or(LibraryPlanError::MissingDownloadSource)?;
        Ok(DownloadJob {
            path: self.relative_path.join_under(&libraries_dir(mc_dir)),
            url,
            name: self.name,
            expected: self.expected,
            allow_missing_checksum: self.allow_missing_checksum,
        })
    }
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
    let jobs = library_jobs_for(mc_dir, libraries, &env, LibraryChecksumPolicy::Strict)?;
    download_library_jobs(jobs, phase, send, None, None).await
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
    let jobs = library_jobs_for(mc_dir, libraries, &env, LibraryChecksumPolicy::Strict)?;
    let (fact_tx, mut fact_rx) = mpsc::unbounded_channel();
    let (descriptor_tx, mut descriptor_rx) = mpsc::unbounded_channel();
    let result = download_library_jobs(jobs, phase, send, Some(fact_tx), Some(descriptor_tx)).await;
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
    let jobs = library_jobs_for(mc_dir, libraries, &env, LibraryChecksumPolicy::AllowMissing)?;
    let (fact_tx, mut fact_rx) = mpsc::unbounded_channel();
    let (descriptor_tx, mut descriptor_rx) = mpsc::unbounded_channel();
    let result = download_library_jobs(jobs, phase, send, Some(fact_tx), Some(descriptor_tx)).await;
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
            if job.allow_missing_checksum {
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

fn resolve_library_plan(
    lib: &Library,
    checksum_policy: LibraryChecksumPolicy,
) -> Result<Option<LibraryArtifactPlan>, LibraryPlanError> {
    if !lib.natives.is_empty()
        && lib
            .downloads
            .as_ref()
            .is_none_or(|downloads| downloads.artifact.is_none())
    {
        return Ok(None);
    }

    if let Some(artifact) = lib
        .downloads
        .as_ref()
        .and_then(|downloads| downloads.artifact.as_ref())
    {
        let relative_path = artifact_relative_path(&artifact.path)?;
        return Ok(Some(LibraryArtifactPlan {
            name: artifact_name(&relative_path, &lib.name),
            relative_path,
            source_url: nonempty_url(&artifact.url),
            expected: library_expected_integrity(lib, artifact.size, &artifact.sha1)?,
            allow_missing_checksum: checksum_policy.allows_missing(),
            is_native: false,
        }));
    }

    let maven_path = maven_to_path(&lib.name);
    if maven_path.as_os_str().is_empty() {
        return Err(LibraryPlanError::InvalidArtifactPath);
    }
    let relative_path = ArtifactRelativePath::from_path(&maven_path)
        .map_err(|_| LibraryPlanError::InvalidArtifactPath)?;
    Ok(Some(LibraryArtifactPlan {
        name: artifact_name(&relative_path, &lib.name),
        source_url: Some(maven_url(lib, &relative_path)),
        relative_path,
        expected: library_expected_integrity(lib, lib.size, &lib.sha1)?,
        allow_missing_checksum: checksum_policy.allows_missing(),
        is_native: false,
    }))
}

fn resolve_native_plan(
    lib: &Library,
    os_name: &str,
    os_arch: &str,
    checksum_policy: LibraryChecksumPolicy,
) -> Result<Option<LibraryArtifactPlan>, LibraryPlanError> {
    let classifier_candidates = native_classifier_candidates(lib, os_name, os_arch);
    for classifier_key in &classifier_candidates {
        if let Some(artifact) = lib
            .downloads
            .as_ref()
            .and_then(|downloads| downloads.classifiers.get(classifier_key))
        {
            let relative_path = artifact_relative_path(&artifact.path)?;
            return Ok(Some(LibraryArtifactPlan {
                name: artifact_name(&relative_path, &format!("{}:{classifier_key}", lib.name)),
                relative_path,
                source_url: nonempty_url(&artifact.url),
                expected: library_expected_integrity(lib, artifact.size, &artifact.sha1)?,
                allow_missing_checksum: checksum_policy.allows_missing(),
                is_native: true,
            }));
        }
    }

    let Some(classifier_key) = classifier_candidates.into_iter().next() else {
        return Ok(None);
    };
    let maven_path = maven_to_path(&format!("{}:{classifier_key}", lib.name));
    if maven_path.as_os_str().is_empty() {
        return Err(LibraryPlanError::InvalidArtifactPath);
    }
    let relative_path = ArtifactRelativePath::from_path(&maven_path)
        .map_err(|_| LibraryPlanError::InvalidArtifactPath)?;
    Ok(Some(LibraryArtifactPlan {
        name: artifact_name(&relative_path, &format!("{}:{classifier_key}", lib.name)),
        source_url: Some(maven_url(lib, &relative_path)),
        relative_path,
        expected: library_expected_integrity(lib, lib.size, &lib.sha1)?,
        allow_missing_checksum: checksum_policy.allows_missing(),
        is_native: true,
    }))
}

fn artifact_relative_path(value: &str) -> Result<ArtifactRelativePath, LibraryPlanError> {
    ArtifactRelativePath::new(value).map_err(|_| LibraryPlanError::InvalidArtifactPath)
}

fn artifact_name(path: &ArtifactRelativePath, fallback: &str) -> String {
    let name = path
        .as_str()
        .rsplit_once('/')
        .map_or(path.as_str(), |(_, name)| name);
    if name.trim().is_empty() {
        fallback.to_string()
    } else {
        name.to_string()
    }
}

fn nonempty_url(value: &str) -> Option<String> {
    (!value.trim().is_empty()).then(|| value.to_string())
}

fn maven_url(lib: &Library, path: &ArtifactRelativePath) -> String {
    let base_url = if lib.url.is_empty() {
        "https://libraries.minecraft.net/".to_string()
    } else if lib.url.ends_with('/') {
        lib.url.clone()
    } else {
        format!("{}/", lib.url)
    };
    format!("{base_url}{}", path.as_str())
}

fn library_expected_integrity(
    lib: &Library,
    size: i64,
    sha1: &str,
) -> Result<ExpectedIntegrity, LibraryPlanError> {
    let mut legacy_sha1 = None;
    for checksum in lib.checksums.iter().map(|checksum| checksum.trim()) {
        if checksum.is_empty() {
            continue;
        }
        if !is_sha1_hex(checksum) {
            return Err(LibraryPlanError::InvalidChecksum);
        }
        legacy_sha1.get_or_insert(checksum);
    }

    let sha1 = if sha1.trim().is_empty() {
        lib.sha1.trim()
    } else {
        sha1.trim()
    };
    if !sha1.is_empty() {
        if !is_sha1_hex(sha1) {
            return Err(LibraryPlanError::InvalidChecksum);
        }
        return Ok(ExpectedIntegrity::from_mojang(size, sha1));
    }

    Ok(match legacy_sha1 {
        Some(checksum) => ExpectedIntegrity {
            size: u64::try_from(size).ok().filter(|value| *value > 0),
            sha1: Some(checksum.to_string()),
        },
        None => ExpectedIntegrity::from_mojang(size, ""),
    })
}

fn native_classifier_candidates(lib: &Library, os_name: &str, os_arch: &str) -> Vec<String> {
    let Some(base) = lib.natives.get(os_name) else {
        return Vec::new();
    };

    let mut candidates = Vec::new();
    let variants = match os_arch {
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
        _ => vec![base.replace("${arch}", os_arch)],
    };

    for variant in variants {
        if !variant.is_empty() && !candidates.contains(&variant) {
            candidates.push(variant);
        }
    }

    candidates
}

pub(crate) fn library_jobs_for(
    mc_dir: &Path,
    libraries: &[Library],
    env: &Environment,
    checksum_policy: LibraryChecksumPolicy,
) -> Result<Vec<DownloadJob>, LibraryPlanError> {
    library_artifact_plans_for(libraries, env, checksum_policy)?
        .into_iter()
        .map(|plan| plan.into_download_job(mc_dir))
        .collect()
}

pub fn library_verification_plans_for(
    mc_dir: &Path,
    libraries: &[Library],
    env: &Environment,
    known_good: Option<&crate::known_good::KnownGoodInventoryAuthority>,
) -> Result<Vec<LibraryVerificationPlan>, LibraryPlanError> {
    Ok(
        library_artifact_plans_for(libraries, env, LibraryChecksumPolicy::Strict)?
            .into_iter()
            .map(|plan| plan.into_verification_plan(mc_dir, known_good))
            .collect(),
    )
}

pub(crate) fn library_artifact_plans_for(
    libraries: &[Library],
    env: &Environment,
    checksum_policy: LibraryChecksumPolicy,
) -> Result<Vec<LibraryArtifactPlan>, LibraryPlanError> {
    let mut plans = BTreeMap::new();

    for lib in libraries {
        if !evaluate_rules(&lib.rules, env) {
            continue;
        }

        if crate::rules::is_native_library(&lib.name) && !native_name_matches_env(&lib.name, env) {
            continue;
        }

        if let Some(plan) = resolve_library_plan(lib, checksum_policy)? {
            insert_plan(&mut plans, plan)?;
        }
        if let Some(plan) = resolve_native_plan(lib, &env.os_name, &env.os_arch, checksum_policy)? {
            insert_plan(&mut plans, plan)?;
        }
    }

    Ok(plans.into_values().collect())
}

fn insert_plan(
    plans: &mut BTreeMap<ArtifactRelativePath, LibraryArtifactPlan>,
    plan: LibraryArtifactPlan,
) -> Result<(), LibraryPlanError> {
    if let Some(existing) = plans.get(&plan.relative_path) {
        return if existing == &plan {
            Ok(())
        } else {
            Err(LibraryPlanError::ConflictingArtifactPath)
        };
    }
    plans.insert(plan.relative_path.clone(), plan);
    Ok(())
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
