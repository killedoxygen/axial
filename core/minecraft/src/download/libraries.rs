use super::client::{library_download_concurrency, standard_minecraft_download_client};
use super::facts::selected_download_source_label;
use super::integrity::is_sha1_hex;
use super::library_source::{
    LIBRARY_SOURCE_MAX_BYTES, LibraryComponentSourceKind, LibrarySourcePool, LibrarySourceRequest,
    RetainedLibraryComponentSource, acquire_retained_library_component_source,
};
use super::model::{
    DownloadError, DownloadProgress, ExactLibraryDownloadProof, ExecutionDownloadFact,
    ExpectedIntegrity, LibraryPlanError, SelectedDownloadArtifactKind, progress,
};
use crate::artifact_path::ArtifactRelativePath;
use crate::known_good_libraries::{
    ClassifiedLibraryDownload, LibraryAcquisition, PendingExactLibraryDeclarations,
    PendingStreamedLibraryDeclarations, SealedLibraryDeclarationError,
};
use crate::launch::{Library, maven_to_path};
use crate::loaders::types::LoaderError;
use crate::managed_fs::{ManagedBoundedFileReader, ManagedDir};
use crate::paths::libraries_dir;
use crate::rules::{Environment, evaluate_rules};
use futures_util::StreamExt;
use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use tokio::sync::mpsc;
#[derive(Debug, Clone)]
pub(crate) struct DownloadJob {
    pub(crate) relative_path: ArtifactRelativePath,
    pub(crate) path: PathBuf,
    pub(crate) url: String,
    pub(crate) name: String,
    pub(crate) expected: ExpectedIntegrity,
    pub(crate) is_native: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LibraryVerificationPlan {
    pub path: PathBuf,
    pub name: String,
    pub integrity: LibraryVerificationIntegrity,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LibraryVerificationIntegrity {
    Sha1(ExpectedIntegrity),
    MissingChecksum,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LibraryArtifactPlan {
    pub(crate) relative_path: ArtifactRelativePath,
    pub(crate) source_url: Option<String>,
    pub(crate) name: String,
    pub(crate) expected: ExpectedIntegrity,
    pub(crate) is_native: bool,
}

impl LibraryArtifactPlan {
    fn into_verification_plan(self, mc_dir: &Path) -> LibraryVerificationPlan {
        let integrity = if self.expected.sha1.is_some() {
            LibraryVerificationIntegrity::Sha1(self.expected.clone())
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
            relative_path: self.relative_path.clone(),
            path: self.relative_path.join_under(&libraries_dir(mc_dir)),
            url,
            name: self.name,
            expected: self.expected,
            is_native: self.is_native,
        })
    }
}

#[derive(Clone)]
pub(crate) struct ExactLibraryCacheAdmission {
    libraries: Option<ManagedDir>,
}

struct ExactLibraryCacheSource {
    reader: ManagedBoundedFileReader,
}

struct ExactLibraryCacheGuard {
    directory: ManagedDir,
    name: String,
    guard: crate::managed_fs::ManagedFileGuard,
}

impl ExactLibraryCacheAdmission {
    pub(crate) async fn bind(managed_root: &Path) -> Result<Self, DownloadError> {
        let managed_root = managed_root.to_path_buf();
        tokio::task::spawn_blocking(move || {
            let root = match ManagedDir::open_root(&managed_root) {
                Ok(root) => root,
                Err(LoaderError::Io(error)) if error.kind() == io::ErrorKind::NotFound => {
                    return Ok(Self { libraries: None });
                }
                Err(error) => return Err(cache_admission_error(error)),
            };
            if !root
                .has_portably_exact_child_name("libraries")
                .map_err(cache_admission_error)?
            {
                return Ok(Self { libraries: None });
            }
            let libraries = root
                .open_child("libraries")
                .map_err(cache_admission_error)?;
            Ok(Self {
                libraries: Some(libraries),
            })
        })
        .await
        .map_err(|error| {
            DownloadError::FileOperation(io::Error::other(format!(
                "library cache admission task stopped unexpectedly: {error}"
            )))
        })?
    }

    pub(crate) async fn requires_retained_source(
        &self,
        job: &DownloadJob,
    ) -> Result<bool, DownloadError> {
        let expected_size = job.expected.size.ok_or(LibraryPlanError::InvalidChecksum)?;
        let expected_sha1 = job
            .expected
            .sha1
            .as_deref()
            .and_then(decode_sha1)
            .ok_or(LibraryPlanError::InvalidChecksum)?;
        let Some(source) = self.exact_guard(job).await? else {
            return Ok(true);
        };
        tokio::task::spawn_blocking(move || {
            let observed = source
                .directory
                .sha1_guarded_file_bytes(&source.name, &source.guard, expected_size)
                .map_err(cache_admission_error)?;
            Ok(observed != expected_sha1)
        })
        .await
        .map_err(|error| {
            DownloadError::FileOperation(io::Error::other(format!(
                "library cache authentication task stopped unexpectedly: {error}"
            )))
        })?
    }

    async fn retain_installer_source(
        &self,
        job: &DownloadJob,
        source_pool: &LibrarySourcePool,
        kind: LibraryComponentSourceKind,
    ) -> Result<Option<RetainedLibraryComponentSource>, DownloadError> {
        let Some(source) = self.exact_source(job).await? else {
            return Ok(None);
        };
        let ExactLibraryCacheSource { reader } = source;
        let expected_size = job.expected.size.ok_or(LibraryPlanError::InvalidChecksum)?;
        let expected_sha1 = job
            .expected
            .sha1
            .as_deref()
            .and_then(decode_sha1)
            .ok_or(LibraryPlanError::InvalidChecksum)?;
        let Some(allocation) = source_pool
            .try_retain_authenticated_component_reader(reader, expected_size, expected_sha1)
            .await?
        else {
            return Ok(None);
        };
        Ok(Some(
            RetainedLibraryComponentSource::from_authenticated_allocation(
                allocation,
                job.relative_path.clone(),
                expected_size,
                expected_sha1,
                job.expected.clone(),
                job.url.clone(),
                kind,
            ),
        ))
    }

    async fn exact_source(
        &self,
        job: &DownloadJob,
    ) -> Result<Option<ExactLibraryCacheSource>, DownloadError> {
        let Some(source) = self.exact_guard(job).await? else {
            return Ok(None);
        };
        let expected_size = job.expected.size.ok_or(LibraryPlanError::InvalidChecksum)?;
        let reader = source
            .guard
            .into_bounded_reader(expected_size)
            .map_err(cache_admission_error)?;
        Ok(Some(ExactLibraryCacheSource { reader }))
    }

    async fn exact_guard(
        &self,
        job: &DownloadJob,
    ) -> Result<Option<ExactLibraryCacheGuard>, DownloadError> {
        let expected_size = job.expected.size.ok_or(LibraryPlanError::InvalidChecksum)?;
        if expected_size == 0 || expected_size > LIBRARY_SOURCE_MAX_BYTES {
            return Err(DownloadError::Integrity(
                "exact library cache contract exceeds the admitted size bound".to_string(),
            ));
        }
        if job.expected.sha1.as_deref().and_then(decode_sha1).is_none() {
            return Err(LibraryPlanError::InvalidChecksum.into());
        }
        let Some(libraries) = self.libraries.clone() else {
            return Ok(None);
        };
        let relative_path = job.relative_path.clone();
        tokio::task::spawn_blocking(move || {
            let mut segments = relative_path.as_str().split('/').peekable();
            let mut directory = libraries;
            while let Some(segment) = segments.next() {
                if segments.peek().is_none() {
                    if !directory
                        .has_portably_exact_child_name(segment)
                        .map_err(cache_admission_error)?
                    {
                        return Ok(None);
                    }
                    let guard = directory
                        .inspect_regular_file(segment)
                        .map_err(cache_admission_error)?
                        .ok_or_else(|| {
                            DownloadError::Integrity(
                                "library cache identity changed during admission".to_string(),
                            )
                        })?;
                    if guard.size() != expected_size {
                        return Ok(None);
                    }
                    return Ok(Some(ExactLibraryCacheGuard {
                        directory,
                        name: segment.to_string(),
                        guard,
                    }));
                }
                if !directory
                    .has_portably_exact_child_name(segment)
                    .map_err(cache_admission_error)?
                {
                    return Ok(None);
                }
                directory = directory
                    .open_child(segment)
                    .map_err(cache_admission_error)?;
            }
            Err(DownloadError::Integrity(
                "library cache admission received an empty artifact path".to_string(),
            ))
        })
        .await
        .map_err(|error| {
            DownloadError::FileOperation(io::Error::other(format!(
                "library cache inspection task stopped unexpectedly: {error}"
            )))
        })?
    }
}

fn cache_admission_error(error: LoaderError) -> DownloadError {
    DownloadError::Integrity(format!("library cache admission failed: {error}"))
}

pub(super) struct RetainedClassifiedLibraryAcquisition {
    pub(super) relative_path: ArtifactRelativePath,
    pub(super) name: String,
    pub(super) observed_size: u64,
    pub(super) proof: Option<ExactLibraryDownloadProof>,
    pub(super) source: Option<RetainedLibraryComponentSource>,
}

pub(super) async fn acquire_retained_classified_library(
    client: &reqwest::Client,
    classified: ClassifiedLibraryDownload,
    cache_admission: &ExactLibraryCacheAdmission,
    source_pool: &LibrarySourcePool,
    fact_tx: Option<&mpsc::UnboundedSender<ExecutionDownloadFact>>,
) -> Result<RetainedClassifiedLibraryAcquisition, DownloadError> {
    let (job, acquisition) = classified.into_parts();
    let requires_source = match acquisition {
        LibraryAcquisition::ExactDeclaration => {
            cache_admission.requires_retained_source(&job).await?
        }
        LibraryAcquisition::FreshStream => true,
    };
    let (observed_size, proof, source) = if requires_source {
        let target = selected_download_source_label(
            SelectedDownloadArtifactKind::Library,
            job.relative_path.as_str(),
        );
        let source = acquire_retained_library_component_source(
            LibrarySourceRequest {
                client,
                url: &job.url,
                expected: &job.expected,
                relative_path: &job.relative_path,
                max_bytes: LIBRARY_SOURCE_MAX_BYTES,
                target: &target,
                pool: source_pool,
                fact_tx,
            },
            if job.is_native {
                LibraryComponentSourceKind::NativeLibrary
            } else {
                LibraryComponentSourceKind::Library
            },
        )
        .await?;
        let observed_size = source.observed_size();
        let proof =
            (acquisition == LibraryAcquisition::FreshStream).then(|| source.exact_download_proof());
        (observed_size, proof, Some(source))
    } else {
        (job.expected.size.unwrap_or(0), None, None)
    };
    Ok(RetainedClassifiedLibraryAcquisition {
        relative_path: job.relative_path,
        name: job.name,
        observed_size,
        proof,
        source,
    })
}

async fn acquire_retained_installer_library(
    client: &reqwest::Client,
    classified: ClassifiedLibraryDownload,
    cache_admission: &ExactLibraryCacheAdmission,
    source_pool: &LibrarySourcePool,
    fact_tx: Option<&mpsc::UnboundedSender<ExecutionDownloadFact>>,
) -> Result<(ArtifactRelativePath, String, RetainedLibraryComponentSource), DownloadError> {
    let (job, acquisition) = classified.into_parts();
    let kind = if job.is_native {
        LibraryComponentSourceKind::NativeLibrary
    } else {
        LibraryComponentSourceKind::Library
    };
    let cached = if acquisition == LibraryAcquisition::ExactDeclaration {
        cache_admission
            .retain_installer_source(&job, source_pool, kind)
            .await?
    } else {
        None
    };
    let source = match cached {
        Some(source) => source,
        None => {
            let target = selected_download_source_label(
                SelectedDownloadArtifactKind::Library,
                job.relative_path.as_str(),
            );
            acquire_retained_library_component_source(
                LibrarySourceRequest {
                    client,
                    url: &job.url,
                    expected: &job.expected,
                    relative_path: &job.relative_path,
                    max_bytes: LIBRARY_SOURCE_MAX_BYTES,
                    target: &target,
                    pool: source_pool,
                    fact_tx,
                },
                kind,
            )
            .await?
        }
    };
    Ok((job.relative_path, job.name, source))
}

pub(crate) async fn download_profile_retained_libraries_with_declarations_and_facts<F, G>(
    mc_dir: &Path,
    declarations: PendingExactLibraryDeclarations,
    phase: &str,
    send: F,
    mut send_fact: G,
) -> Result<
    (
        PendingStreamedLibraryDeclarations,
        Vec<ExactLibraryDownloadProof>,
        Vec<RetainedLibraryComponentSource>,
    ),
    DownloadError,
>
where
    F: FnMut(DownloadProgress),
    G: FnMut(ExecutionDownloadFact),
{
    let jobs = {
        let (libraries, environment) = declarations.profile_plan_inputs().ok_or_else(|| {
            profile_declaration_error(SealedLibraryDeclarationError::AncestorMismatch)
        })?;
        library_jobs_for(mc_dir, libraries, environment)?
    };
    let (declarations, jobs) = declarations
        .classify_jobs(&libraries_dir(mc_dir), jobs)
        .map_err(profile_declaration_error)?;
    let cache_admission = ExactLibraryCacheAdmission::bind(mc_dir).await?;
    let (fact_tx, mut fact_rx) = mpsc::unbounded_channel();
    let result =
        download_profile_retained_library_jobs(jobs, cache_admission, phase, send, Some(fact_tx))
            .await;
    while let Ok(fact) = fact_rx.try_recv() {
        send_fact(fact);
    }
    result.map(|(proofs, sources)| (declarations, proofs, sources))
}

fn profile_declaration_error(error: SealedLibraryDeclarationError) -> DownloadError {
    DownloadError::ResolveManifest(format!(
        "profile library declaration classification failed: {error:?}"
    ))
}

pub(crate) async fn download_installer_libraries_with_declarations_and_facts<F, G>(
    mc_dir: &Path,
    install: crate::loaders::PendingForgeNetworkInstall,
    phase: &str,
    send: F,
    mut send_fact: G,
) -> Result<
    (
        crate::loaders::PendingForgeInstallExecution,
        Vec<RetainedLibraryComponentSource>,
    ),
    DownloadError,
>
where
    F: FnMut(DownloadProgress),
    G: FnMut(ExecutionDownloadFact),
{
    let (pending_execution, jobs) = install.into_parts();
    let (fact_tx, mut fact_rx) = mpsc::unbounded_channel();
    let result =
        download_installer_classified_library_jobs(mc_dir, jobs, phase, send, Some(fact_tx)).await;
    while let Ok(fact) = fact_rx.try_recv() {
        send_fact(fact);
    }
    result.map(|materialized| (pending_execution, materialized))
}

async fn download_profile_retained_library_jobs<F>(
    jobs: Vec<ClassifiedLibraryDownload>,
    cache_admission: ExactLibraryCacheAdmission,
    phase: &str,
    mut send: F,
    fact_tx: Option<mpsc::UnboundedSender<ExecutionDownloadFact>>,
) -> Result<
    (
        Vec<ExactLibraryDownloadProof>,
        Vec<RetainedLibraryComponentSource>,
    ),
    DownloadError,
>
where
    F: FnMut(DownloadProgress),
{
    let client = standard_minecraft_download_client();
    let source_pool = LibrarySourcePool::for_component_retention()?;
    send(progress(phase, 0, jobs.len() as i32, None));
    let total_jobs = jobs.len() as i32;
    let mut completed_jobs = 0;
    let mut proofs = Vec::new();
    let mut sources = BTreeMap::new();
    let mut downloads = futures_util::stream::iter(jobs.into_iter().map(|classified| {
        let client = client.clone();
        let fact_tx = fact_tx.clone();
        let source_pool = source_pool.clone();
        let cache_admission = cache_admission.clone();
        async move {
            acquire_retained_classified_library(
                &client,
                classified,
                &cache_admission,
                &source_pool,
                fact_tx.as_ref(),
            )
            .await
        }
    }))
    .buffer_unordered(library_download_concurrency());
    while let Some(result) = downloads.next().await {
        let RetainedClassifiedLibraryAcquisition {
            relative_path,
            name,
            observed_size: _,
            proof,
            source,
        } = result?;
        completed_jobs += 1;
        send(progress(phase, completed_jobs, total_jobs, Some(name)));
        if let Some(proof) = proof {
            proofs.push(proof);
        }
        if let Some(source) = source
            && sources.insert(relative_path, source).is_some()
        {
            return Err(LibraryPlanError::ConflictingArtifactPath.into());
        }
    }
    Ok((proofs, sources.into_values().collect()))
}

async fn download_installer_classified_library_jobs<F>(
    mc_dir: &Path,
    jobs: Vec<ClassifiedLibraryDownload>,
    phase: &str,
    mut send: F,
    fact_tx: Option<mpsc::UnboundedSender<ExecutionDownloadFact>>,
) -> Result<Vec<RetainedLibraryComponentSource>, DownloadError>
where
    F: FnMut(DownloadProgress),
{
    let client = standard_minecraft_download_client();
    let source_pool = LibrarySourcePool::for_component_retention()?;
    let cache_admission = ExactLibraryCacheAdmission::bind(mc_dir).await?;
    send(progress(phase, 0, jobs.len() as i32, None));
    let total_jobs = jobs.len() as i32;
    let mut completed_jobs = 0;
    let mut sources = BTreeMap::new();
    let mut downloads = futures_util::stream::iter(jobs.into_iter().map(|classified| {
        let client = client.clone();
        let fact_tx = fact_tx.clone();
        let source_pool = source_pool.clone();
        let cache_admission = cache_admission.clone();
        async move {
            acquire_retained_installer_library(
                &client,
                classified,
                &cache_admission,
                &source_pool,
                fact_tx.as_ref(),
            )
            .await
        }
    }))
    .buffer_unordered(library_download_concurrency());
    while let Some(result) = downloads.next().await {
        let (path, name, source) = result?;
        completed_jobs += 1;
        send(progress(phase, completed_jobs, total_jobs, Some(name)));
        if sources.insert(path, source).is_some() {
            return Err(LibraryPlanError::ConflictingArtifactPath.into());
        }
    }
    Ok(sources.into_values().collect())
}

pub(crate) fn decode_sha1(value: &str) -> Option<[u8; 20]> {
    if !is_sha1_hex(value) {
        return None;
    }
    let mut digest = [0_u8; 20];
    for (index, pair) in value.as_bytes().chunks_exact(2).enumerate() {
        digest[index] = hex_nibble(pair[0])?
            .checked_mul(16)?
            .checked_add(hex_nibble(pair[1])?)?;
    }
    Some(digest)
}

fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn resolve_library_plan(lib: &Library) -> Result<Option<LibraryArtifactPlan>, LibraryPlanError> {
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
            expected: library_expected_integrity(lib, artifact.size, &artifact.sha1, true)?,
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
        expected: library_expected_integrity(lib, lib.size, &lib.sha1, true)?,
        is_native: false,
    }))
}

fn resolve_native_plan(
    lib: &Library,
    os_name: &str,
    os_arch: &str,
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
                expected: library_expected_integrity(lib, artifact.size, &artifact.sha1, false)?,
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
        expected: library_expected_integrity(lib, 0, "", false)?,
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
    compare_top_level: bool,
) -> Result<ExpectedIntegrity, LibraryPlanError> {
    if !compare_top_level {
        let sha1 = sha1.trim();
        if !sha1.is_empty() && !is_sha1_hex(sha1) {
            return Err(LibraryPlanError::InvalidChecksum);
        }
        return Ok(ExpectedIntegrity::from_mojang(size, sha1));
    }
    if compare_top_level
        && ((!sha1.trim().is_empty()
            && !lib.sha1.trim().is_empty()
            && !sha1.trim().eq_ignore_ascii_case(lib.sha1.trim()))
            || (size > 0 && lib.size > 0 && size != lib.size))
    {
        return Err(LibraryPlanError::ConflictingArtifactIntegrity);
    }
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
) -> Result<Vec<DownloadJob>, LibraryPlanError> {
    library_artifact_plans_for(libraries, env)?
        .into_iter()
        .map(|plan| plan.into_download_job(mc_dir))
        .collect()
}

pub fn library_verification_plans_for(
    mc_dir: &Path,
    libraries: &[Library],
    env: &Environment,
) -> Result<Vec<LibraryVerificationPlan>, LibraryPlanError> {
    Ok(library_artifact_plans_for(libraries, env)?
        .into_iter()
        .map(|plan| plan.into_verification_plan(mc_dir))
        .collect())
}

pub(crate) fn library_artifact_plans_for(
    libraries: &[Library],
    env: &Environment,
) -> Result<Vec<LibraryArtifactPlan>, LibraryPlanError> {
    let mut plans = BTreeMap::new();

    for lib in libraries {
        if !evaluate_rules(&lib.rules, env) {
            continue;
        }

        if crate::rules::is_native_library(&lib.name) && !native_name_matches_env(&lib.name, env) {
            continue;
        }

        if let Some(plan) = resolve_library_plan(lib)? {
            insert_plan(&mut plans, plan)?;
        }
        if let Some(plan) = resolve_native_plan(lib, &env.os_name, &env.os_arch)? {
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

#[cfg(test)]
mod tests {
    use super::{
        DownloadJob, ExactLibraryCacheAdmission, acquire_retained_installer_library,
        library_artifact_plans_for,
    };
    use crate::artifact_path::ArtifactRelativePath;
    use crate::download::ExpectedIntegrity;
    use crate::download::library_source::LibrarySourcePool;
    use crate::known_good_libraries::{ClassifiedLibraryDownload, LibraryAcquisition};
    use crate::launch::{Library, LibraryArtifact, LibraryDownload};
    use sha1::{Digest as _, Sha1};
    use std::collections::HashMap;
    use std::fs;
    use std::io::{Read as _, Write as _};
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    use tokio::sync::oneshot;

    static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    fn direct_library(path: &str, url: &str, sha1: &str, size: i64) -> Library {
        Library {
            name: "org.example:fixture:1".to_string(),
            downloads: Some(LibraryDownload {
                artifact: Some(LibraryArtifact {
                    path: path.to_string(),
                    url: url.to_string(),
                    sha1: sha1.to_string(),
                    size,
                }),
                classifiers: HashMap::new(),
            }),
            ..Library::default()
        }
    }

    fn temp_root(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "axial-library-admission-{label}-{}-{}",
            std::process::id(),
            TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ))
    }

    fn exact_job(root: &Path, bytes: &[u8]) -> DownloadJob {
        let relative_path =
            ArtifactRelativePath::new("org/example/exact/1/exact-1.jar").expect("artifact path");
        DownloadJob {
            path: relative_path.join_under(&root.join("libraries")),
            relative_path,
            url: "https://example.invalid/exact.jar".to_string(),
            name: "exact-1.jar".to_string(),
            expected: ExpectedIntegrity {
                size: Some(bytes.len() as u64),
                sha1: Some(format!("{:x}", Sha1::digest(bytes))),
            },
            is_native: false,
        }
    }

    fn jar_bytes(payload: &[u8]) -> Vec<u8> {
        let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        writer
            .start_file(
                "fixture/Library.class",
                zip::write::SimpleFileOptions::default(),
            )
            .expect("start test JAR entry");
        writer.write_all(payload).expect("write test JAR entry");
        writer.finish().expect("finish test JAR").into_inner()
    }

    async fn retained_test_server(
        body: Vec<u8>,
    ) -> (
        String,
        Arc<AtomicUsize>,
        oneshot::Sender<()>,
        tokio::task::JoinHandle<()>,
    ) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind retained test server");
        let address = listener.local_addr().expect("retained test address");
        let requests = Arc::new(AtomicUsize::new(0));
        let server_requests = Arc::clone(&requests);
        let (stop_tx, mut stop_rx) = oneshot::channel();
        let task = tokio::spawn(async move {
            loop {
                let accepted = tokio::select! {
                    _ = &mut stop_rx => break,
                    accepted = listener.accept() => accepted,
                };
                let Ok((mut stream, _)) = accepted else {
                    break;
                };
                let mut request = [0_u8; 4096];
                let _ = stream.read(&mut request).await;
                server_requests.fetch_add(1, Ordering::SeqCst);
                let header = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                if stream.write_all(header.as_bytes()).await.is_err() {
                    continue;
                }
                let _ = stream.write_all(&body).await;
            }
        });
        (
            format!("http://{address}/library.jar"),
            requests,
            stop_tx,
            task,
        )
    }

    #[test]
    fn library_plans_preserve_checksumless_metadata() {
        let library = direct_library(
            "org/example/checksumless/1/checksumless-1.jar",
            "https://example.invalid/checksumless.jar",
            "",
            0,
        );
        let plans = library_artifact_plans_for(&[library], &crate::rules::default_environment())
            .expect("checksumless plan");
        assert_eq!(plans.len(), 1);
        assert!(plans[0].expected.sha1.is_none());
    }

    #[tokio::test]
    async fn exact_cache_admission_treats_missing_root_and_library_tree_as_misses() {
        let root = temp_root("missing");
        let job = exact_job(&root, b"exact bytes");
        let missing_root = ExactLibraryCacheAdmission::bind(&root)
            .await
            .expect("bind missing root");
        assert!(
            missing_root
                .requires_retained_source(&job)
                .await
                .expect("inspect missing root")
        );
        drop(missing_root);

        fs::create_dir_all(&root).expect("create managed root");
        let missing_libraries = ExactLibraryCacheAdmission::bind(&root)
            .await
            .expect("bind missing Libraries tree");
        assert!(
            missing_libraries
                .requires_retained_source(&job)
                .await
                .expect("inspect missing Libraries tree")
        );
        drop(missing_libraries);
        fs::remove_dir_all(root).expect("remove missing-tree test root");
    }

    #[tokio::test]
    async fn exact_cache_admission_omits_exact_and_retains_corrupt_files() {
        let root = temp_root("exact-corrupt");
        let bytes = b"exact library bytes";
        let job = exact_job(&root, bytes);
        fs::create_dir_all(job.path.parent().expect("artifact parent"))
            .expect("create artifact parent");
        fs::write(&job.path, bytes).expect("write exact library");
        let admission = ExactLibraryCacheAdmission::bind(&root)
            .await
            .expect("bind exact Libraries tree");
        assert!(
            !admission
                .requires_retained_source(&job)
                .await
                .expect("admit exact library")
        );

        let corrupt = b"wrong library bytes";
        assert_eq!(corrupt.len(), bytes.len(), "exercise guarded hashing");
        fs::write(&job.path, corrupt).expect("replace same-size corrupt library");
        assert!(
            admission
                .requires_retained_source(&job)
                .await
                .expect("classify corrupt library")
        );
        drop(admission);
        fs::remove_dir_all(root).expect("remove exact-corrupt test root");
    }

    #[tokio::test]
    async fn corrupt_installer_exact_cache_falls_back_to_retention_without_canonical_prewrite() {
        let root = temp_root("installer-corrupt-fallback");
        let body = jar_bytes(b"authenticated installer source");
        let mut corrupt = body.clone();
        corrupt[0] ^= 0xff;
        let mut job = exact_job(&root, &body);
        fs::create_dir_all(job.path.parent().expect("artifact parent"))
            .expect("create artifact parent");
        fs::write(&job.path, &corrupt).expect("seed same-size corrupt exact cache");
        let (url, requests, stop, task) = retained_test_server(body.clone()).await;
        job.url = url;
        let admission = ExactLibraryCacheAdmission::bind(&root)
            .await
            .expect("bind installer exact cache");
        let pool = LibrarySourcePool::for_component_retention().expect("retained source pool");
        let (_, _, source) = acquire_retained_installer_library(
            &reqwest::Client::new(),
            ClassifiedLibraryDownload::from_test(job.clone(), LibraryAcquisition::ExactDeclaration),
            &admission,
            &pool,
            None,
        )
        .await
        .expect("fallback retained installer source");

        assert_eq!(requests.load(Ordering::SeqCst), 1);
        assert_eq!(
            fs::read(&job.path).expect("canonical corrupt cache"),
            corrupt
        );
        assert_eq!(
            pool.retained_available_bytes(),
            Some(crate::known_good::MAX_TIER2_AGGREGATE_BYTES - body.len() as u64)
        );
        let (mut replay, size, sha1) = source
            .replay()
            .expect("replay retained fallback")
            .into_parts();
        let mut replayed = Vec::new();
        replay
            .read_to_end(&mut replayed)
            .expect("read retained fallback");
        assert_eq!(replayed, body);
        assert_eq!(size, body.len() as u64);
        assert_eq!(sha1, <[u8; 20]>::from(Sha1::digest(&body)));

        let _ = stop.send(());
        task.await.expect("stop retained test server");
        drop(admission);
        fs::remove_dir_all(root).expect("remove corrupt-fallback root");
    }

    #[tokio::test]
    async fn exact_cache_admission_rejects_invalid_final_topology() {
        let root = temp_root("invalid-topology");
        let job = exact_job(&root, b"exact library bytes");
        fs::create_dir_all(&job.path).expect("create directory at artifact path");
        let admission = ExactLibraryCacheAdmission::bind(&root)
            .await
            .expect("bind Libraries tree");
        let error = admission
            .requires_retained_source(&job)
            .await
            .expect_err("directory final must fail closed");
        assert!(error.to_string().contains("unsupported type"));
        drop(admission);
        fs::remove_dir_all(root).expect("remove invalid-topology test root");
    }
}
