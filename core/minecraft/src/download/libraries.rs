use super::client::{library_download_concurrency, standard_minecraft_download_client};
use super::facts::selected_download_target_label;
use super::integrity::is_sha1_hex;
use super::library_source::{
    LIBRARY_SOURCE_MAX_BYTES, LibrarySourcePool, LibrarySourceRequest,
    acquire_authenticated_library_source,
};
use super::model::{
    DownloadError, DownloadProgress, ExactLibraryDownloadProof, ExecutionDownloadFact,
    ExpectedIntegrity, LibraryPlanError, SelectedDownloadArtifactDescriptor,
    SelectedDownloadArtifactKind, progress,
};
use super::transfer::{
    ensure_selected_artifact_with_client_and_observed_size,
    materialize_authenticated_library_source, prepare_library_publication,
};
use crate::artifact_path::ArtifactRelativePath;
use crate::known_good_libraries::{
    ClassifiedLibraryDownload, LibraryAcquisition, PendingExactLibraryDeclarations,
    PendingStreamedLibraryDeclarations, SealedLibraryDeclarationError,
};
use crate::launch::{Library, maven_to_path};
use crate::paths::libraries_dir;
use crate::rules::{Environment, default_environment, evaluate_rules};
use futures_util::StreamExt;
use std::collections::{BTreeMap, BTreeSet};
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

pub(crate) async fn download_profile_libraries_with_declarations_and_facts_and_descriptors<
    F,
    G,
    H,
>(
    mc_dir: &Path,
    declarations: PendingExactLibraryDeclarations,
    phase: &str,
    send: F,
    mut send_fact: G,
    mut send_descriptor: H,
) -> Result<
    (
        PendingStreamedLibraryDeclarations,
        Vec<ExactLibraryDownloadProof>,
    ),
    DownloadError,
>
where
    F: FnMut(DownloadProgress),
    G: FnMut(ExecutionDownloadFact),
    H: FnMut(SelectedDownloadArtifactDescriptor),
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
    let (fact_tx, mut fact_rx) = mpsc::unbounded_channel();
    let (descriptor_tx, mut descriptor_rx) = mpsc::unbounded_channel();
    let result = download_classified_library_jobs_with_proofs(
        mc_dir,
        jobs,
        phase,
        send,
        Some(fact_tx),
        Some(descriptor_tx),
        false,
    )
    .await;
    while let Ok(fact) = fact_rx.try_recv() {
        send_fact(fact);
    }
    while let Ok(descriptor) = descriptor_rx.try_recv() {
        send_descriptor(descriptor);
    }
    result.map(|proofs| (declarations, proofs))
}

fn profile_declaration_error(error: SealedLibraryDeclarationError) -> DownloadError {
    DownloadError::ResolveManifest(format!(
        "profile library declaration classification failed: {error:?}"
    ))
}

pub(crate) async fn download_installer_libraries_with_authority_and_facts_and_descriptors<F, G, H>(
    mc_dir: &Path,
    libraries: &[Library],
    excluded_paths: &BTreeSet<ArtifactRelativePath>,
    phase: &str,
    send: F,
    mut send_fact: G,
    mut send_descriptor: H,
) -> Result<Vec<ExactLibraryDownloadProof>, DownloadError>
where
    F: FnMut(DownloadProgress),
    G: FnMut(ExecutionDownloadFact),
    H: FnMut(SelectedDownloadArtifactDescriptor),
{
    let env = default_environment();
    let jobs = installer_library_jobs_for(mc_dir, libraries, &env, excluded_paths)?;
    let (fact_tx, mut fact_rx) = mpsc::unbounded_channel();
    let (descriptor_tx, mut descriptor_rx) = mpsc::unbounded_channel();
    let result = download_library_jobs_with_proofs(
        mc_dir,
        jobs,
        phase,
        send,
        Some(fact_tx),
        Some(descriptor_tx),
    )
    .await;
    while let Ok(fact) = fact_rx.try_recv() {
        send_fact(fact);
    }
    while let Ok(descriptor) = descriptor_rx.try_recv() {
        send_descriptor(descriptor);
    }
    result
}

async fn download_library_jobs_with_proofs<F>(
    mc_dir: &Path,
    jobs: Vec<DownloadJob>,
    phase: &str,
    send: F,
    fact_tx: Option<mpsc::UnboundedSender<ExecutionDownloadFact>>,
    descriptor_tx: Option<mpsc::UnboundedSender<SelectedDownloadArtifactDescriptor>>,
) -> Result<Vec<ExactLibraryDownloadProof>, DownloadError>
where
    F: FnMut(DownloadProgress),
{
    let jobs = jobs
        .into_iter()
        .map(|job| ClassifiedLibraryDownload {
            acquisition: if job.expected.sha1.is_some() && job.expected.size.is_some() {
                LibraryAcquisition::ExactDeclaration
            } else {
                LibraryAcquisition::FreshStream
            },
            job,
        })
        .collect();
    download_classified_library_jobs_with_proofs(
        mc_dir,
        jobs,
        phase,
        send,
        fact_tx,
        descriptor_tx,
        true,
    )
    .await
}

async fn download_classified_library_jobs_with_proofs<F>(
    mc_dir: &Path,
    jobs: Vec<ClassifiedLibraryDownload>,
    phase: &str,
    mut send: F,
    fact_tx: Option<mpsc::UnboundedSender<ExecutionDownloadFact>>,
    descriptor_tx: Option<mpsc::UnboundedSender<SelectedDownloadArtifactDescriptor>>,
    retain_exact_proofs: bool,
) -> Result<Vec<ExactLibraryDownloadProof>, DownloadError>
where
    F: FnMut(DownloadProgress),
{
    let client = standard_minecraft_download_client();
    let source_pool = LibrarySourcePool::new();
    let mc_dir = mc_dir.to_path_buf();
    send(progress(phase, 0, jobs.len() as i32, None));
    let total_jobs = jobs.len() as i32;
    let mut completed_jobs = 0;
    let mut authorities = BTreeMap::new();
    let mut downloads = futures_util::stream::iter(jobs.into_iter().map(|classified| {
        let job = classified.job;
        let acquisition = classified.acquisition;
        let client = client.clone();
        let fact_tx = fact_tx.clone();
        let descriptor_tx = descriptor_tx.clone();
        let source_pool = source_pool.clone();
        let mc_dir = mc_dir.clone();
        async move {
            let (authority, path) = if acquisition == LibraryAcquisition::ExactDeclaration {
                let (_, observed_size) = ensure_selected_artifact_with_client_and_observed_size(
                    SelectedDownloadArtifactKind::Library,
                    &client,
                    &job.url,
                    &job.path,
                    &job.expected,
                    fact_tx.as_ref(),
                    descriptor_tx.as_ref(),
                )
                .await?;
                let sha1 = decode_sha1(
                    job.expected
                        .sha1
                        .as_deref()
                        .ok_or(LibraryPlanError::InvalidChecksum)?,
                )
                .ok_or(LibraryPlanError::InvalidChecksum)?;
                let proof = retain_exact_proofs.then(|| {
                    ExactLibraryDownloadProof::new(
                        job.relative_path.clone(),
                        job.is_native,
                        job.url.clone(),
                        job.expected.clone(),
                        observed_size,
                        sha1,
                    )
                });
                (proof, job.relative_path)
            } else {
                let target = selected_download_target_label(
                    SelectedDownloadArtifactKind::Library,
                    Path::new(job.relative_path.as_str()),
                );
                let source = acquire_authenticated_library_source(LibrarySourceRequest {
                    client: &client,
                    url: &job.url,
                    expected: &job.expected,
                    relative_path: &job.relative_path,
                    max_bytes: LIBRARY_SOURCE_MAX_BYTES,
                    target: &target,
                    pool: &source_pool,
                    fact_tx: fact_tx.as_ref(),
                })
                .await?;
                let prepared = prepare_library_publication(
                    &mc_dir,
                    job.relative_path.clone(),
                    &job.url,
                    &job.expected,
                    job.is_native,
                    fact_tx.as_ref(),
                    descriptor_tx.as_ref(),
                )
                .await?;
                let (authority, _) =
                    materialize_authenticated_library_source(prepared, source, fact_tx.as_ref())
                        .await?;
                (Some(authority), job.relative_path)
            };
            Ok::<_, DownloadError>((path, job.name, authority))
        }
    }))
    .buffer_unordered(library_download_concurrency());
    while let Some(result) = downloads.next().await {
        let (path, name, authority) = result?;
        completed_jobs += 1;
        send(progress(phase, completed_jobs, total_jobs, Some(name)));
        if let Some(authority) = authority
            && authorities.insert(path, authority).is_some()
        {
            return Err(LibraryPlanError::ConflictingArtifactPath.into());
        }
    }
    Ok(authorities.into_values().collect())
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

fn installer_library_jobs_for(
    mc_dir: &Path,
    libraries: &[Library],
    env: &Environment,
    excluded_paths: &BTreeSet<ArtifactRelativePath>,
) -> Result<Vec<DownloadJob>, LibraryPlanError> {
    let plans = library_artifact_plans_for(libraries, env)?;
    let planned_paths = plans
        .iter()
        .map(|plan| plan.relative_path.clone())
        .collect::<BTreeSet<_>>();
    if !excluded_paths.is_subset(&planned_paths) {
        return Err(LibraryPlanError::InvalidArtifactExclusions);
    }
    plans
        .into_iter()
        .filter(|plan| !excluded_paths.contains(&plan.relative_path))
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
mod exact_library_proof_tests {
    use super::{
        download_installer_libraries_with_authority_and_facts_and_descriptors,
        installer_library_jobs_for, library_artifact_plans_for,
    };
    use crate::artifact_path::ArtifactRelativePath;
    use crate::download::LibraryPlanError;
    use crate::launch::{Library, LibraryArtifact, LibraryDownload};
    use sha1::{Digest as _, Sha1};
    use std::collections::{BTreeSet, HashMap};
    use std::io::Write as _;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use zip::ZipWriter;
    use zip::write::SimpleFileOptions;

    #[test]
    fn installer_exclusions_are_exact_and_applied_before_download_source_validation() {
        let path = ArtifactRelativePath::new("org/example/generated/1/generated-1.jar")
            .expect("artifact path");
        let library = direct_library(path.as_str(), "", "", 0);
        let exclusions = BTreeSet::from([path.clone()]);

        let jobs = installer_library_jobs_for(
            Path::new("/tmp/axial-installer-authority"),
            std::slice::from_ref(&library),
            &crate::rules::default_environment(),
            &exclusions,
        )
        .expect("excluded generated artifact does not need a source");
        assert!(jobs.is_empty());

        let unknown =
            BTreeSet::from([
                ArtifactRelativePath::new("org/example/unknown/1/unknown-1.jar")
                    .expect("unknown path"),
            ]);
        assert_eq!(
            installer_library_jobs_for(
                Path::new("/tmp/axial-installer-authority"),
                &[library],
                &crate::rules::default_environment(),
                &unknown,
            )
            .expect_err("unmatched exclusion"),
            LibraryPlanError::InvalidArtifactExclusions
        );
    }

    #[tokio::test]
    async fn sha_only_installer_authority_forces_fresh_stream() {
        let root = temp_dir("sha-only-fresh");
        let relative = "org/example/sha-only/1/sha-only-1.jar";
        let stale = jar_bytes(b"stale");
        let fresh = jar_bytes(b"fresh");
        let sha1 = format!("{:x}", Sha1::digest(&fresh));
        let destination = root.join("libraries").join(relative);
        std::fs::create_dir_all(destination.parent().expect("library parent"))
            .expect("library parent");
        std::fs::write(&destination, stale).expect("existing library");
        let url = spawn_response(fresh.clone()).await;
        let library = direct_library(relative, &url, &sha1, 0);

        let authorities = download_installer_libraries_with_authority_and_facts_and_descriptors(
            &root,
            &[library],
            &BTreeSet::new(),
            "libraries",
            |_| {},
            |_| {},
            |_| {},
        )
        .await
        .expect("fresh SHA-only source");
        let (path, _, _, _, size, digest) = authorities
            .into_iter()
            .next()
            .expect("authority")
            .into_parts();
        assert_eq!(path.as_str(), relative);
        assert_eq!(size, fresh.len() as u64);
        let expected_digest: [u8; 20] = Sha1::digest(&fresh).into();
        assert_eq!(digest, expected_digest);
        assert_eq!(
            std::fs::read(destination).expect("published library"),
            fresh
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn checksumless_installer_authority_forces_fresh_stream_and_promotion() {
        let root = temp_dir("checksumless-fresh");
        let relative = "org/example/fresh/1/fresh-1.jar";
        let destination = root.join("libraries").join(relative);
        std::fs::create_dir_all(destination.parent().expect("library parent"))
            .expect("library parent");
        let stale = jar_bytes(b"stale");
        std::fs::write(&destination, stale).expect("stale usable library");
        let fresh = jar_bytes(b"fresh");
        let url = spawn_response(fresh.clone()).await;
        let library = direct_library(relative, &url, "", 0);

        let authorities = download_installer_libraries_with_authority_and_facts_and_descriptors(
            &root,
            &[library],
            &BTreeSet::new(),
            "libraries",
            |_| {},
            |_| {},
            |_| {},
        )
        .await
        .expect("fresh checksumless transfer");
        let (path, _, _, _, size, digest) = authorities
            .into_iter()
            .next()
            .expect("authority")
            .into_parts();
        assert_eq!(std::fs::read(destination).expect("promoted library"), fresh);
        assert_eq!(path.as_str(), relative);
        assert_eq!(size, fresh.len() as u64);
        let expected_digest: [u8; 20] = Sha1::digest(&fresh).into();
        assert_eq!(digest, expected_digest);

        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn installer_proofs_preserve_native_classifier_identity() {
        let installer_root = temp_dir("installer-native-proof");
        let relative = "org/example/native/1/native-1-platform.jar";
        let installer_body = jar_bytes(b"installer native");
        let installer_url = spawn_response(installer_body).await;
        let installer_proofs =
            download_installer_libraries_with_authority_and_facts_and_descriptors(
                &installer_root,
                &[native_library(relative, &installer_url)],
                &BTreeSet::new(),
                "libraries",
                |_| {},
                |_| {},
                |_| {},
            )
            .await
            .expect("installer native proof");

        assert_eq!(installer_proofs.len(), 1);
        assert_eq!(
            installer_proofs
                .into_iter()
                .next()
                .unwrap()
                .into_parts()
                .0
                .as_str(),
            relative
        );
        let _ = std::fs::remove_dir_all(installer_root);
    }

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

    fn native_library(path: &str, url: &str) -> Library {
        let env = crate::rules::default_environment();
        Library {
            name: "org.example:native:1".to_string(),
            downloads: Some(LibraryDownload {
                artifact: None,
                classifiers: HashMap::from([(
                    "native-test".to_string(),
                    LibraryArtifact {
                        path: path.to_string(),
                        url: url.to_string(),
                        sha1: String::new(),
                        size: 0,
                    },
                )]),
            }),
            natives: HashMap::from([(env.os_name, "native-test".to_string())]),
            ..Library::default()
        }
    }

    fn jar_bytes(payload: &[u8]) -> Vec<u8> {
        let mut writer = ZipWriter::new(std::io::Cursor::new(Vec::new()));
        writer
            .start_file("fixture", SimpleFileOptions::default())
            .expect("jar entry");
        writer.write_all(payload).expect("jar payload");
        writer.finish().expect("finish jar").into_inner()
    }

    async fn spawn_response(body: Vec<u8>) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind response server");
        let url = format!(
            "http://{}/artifact.jar",
            listener.local_addr().expect("address")
        );
        tokio::spawn(async move {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let mut request = [0_u8; 4096];
            let _ = socket.read(&mut request).await;
            let headers = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = socket.write_all(headers.as_bytes()).await;
            let _ = socket.write_all(&body).await;
        });
        url
    }

    fn temp_dir(prefix: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|value| value.as_nanos())
            .unwrap_or_default();
        std::env::temp_dir().join(format!(
            "axial-library-proof-{prefix}-{}-{nanos:x}",
            std::process::id()
        ))
    }

    #[test]
    fn installer_authority_plans_allow_checksumless_metadata() {
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
}
