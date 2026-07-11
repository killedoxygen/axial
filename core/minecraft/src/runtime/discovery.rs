use super::file_download::{
    RuntimeDownloadActual, RuntimeDownloadEvidence, available_runtime_parallelism,
    component_manifest_destination, component_manifest_link_target_path, runtime_filesystem_path,
    verify_runtime_download,
};
use super::layout::{
    java_executable, runtime_cache_dir, runtime_executable_ready, runtime_os_arch,
};
use super::manifest::{COMPONENT_MANIFEST_PROOF_FILE, ComponentManifest};
use super::model::{
    JavaRuntimeInfo, JavaRuntimeLookupError, JavaRuntimeResult, RuntimeId, RuntimeInstallState,
    RuntimeOverride, RuntimeRecord, RuntimeRequirement, RuntimeSource,
};
use super::probe::{JavaRuntimeProbeReceipt, probe_java_runtime_info, probe_java_runtime_receipt};
use super::rosetta::rosetta_required_error_for_current_host;
use crate::launch::{JavaVersion, java_component_for_major};
use crate::paths::runtime_dirs;
use sha1::{Digest as _, Sha1};
use std::io::Read;
use std::path::{Path, PathBuf};

pub fn runtime_requirement(java_version: &JavaVersion) -> RuntimeRequirement {
    RuntimeRequirement {
        required_java: java_version.clone(),
        preferred_component: RuntimeId(preferred_runtime_component(java_version)),
    }
}

pub fn parse_runtime_override(value: &str) -> RuntimeOverride {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        RuntimeOverride::None
    } else if is_known_runtime_component(trimmed) {
        RuntimeOverride::Component(RuntimeId(trimmed.to_string()))
    } else {
        RuntimeOverride::ExecutablePath(PathBuf::from(trimmed))
    }
}

pub fn list_runtime_records(library_dir: &Path) -> Vec<RuntimeRecord> {
    let components = known_runtime_components();
    let mut dirs = runtime_dirs(library_dir);
    dirs.push(runtime_cache_dir());

    let mut results = Vec::new();
    for dir in dirs {
        for component in &components {
            if let Some(runtime) = inspect_component_runtime(&dir, component)
                && runtime.install_state == RuntimeInstallState::Ready
                && !results.iter().any(|entry: &RuntimeRecord| {
                    entry.id == runtime.id && entry.java_path == runtime.java_path
                })
            {
                results.push(runtime);
            }
        }
    }

    results
}

pub fn list_java_runtimes(library_dir: &Path) -> Vec<JavaRuntimeResult> {
    list_runtime_records(library_dir)
        .into_iter()
        .filter(|record| record.install_state == RuntimeInstallState::Ready)
        .map(|record| JavaRuntimeResult {
            path: record.java_path,
            component: record.id.0,
            source: record.source.as_str().to_string(),
        })
        .collect()
}

pub fn runtime_component_ready_without_probe(library_dir: &Path, component: &str) -> bool {
    let mut dirs = runtime_dirs(library_dir);
    dirs.push(runtime_cache_dir());
    dirs.into_iter()
        .any(|dir| component_runtime_ready_without_probe(&dir, component))
}

pub fn runtime_component_executable_present_without_probe(
    library_dir: &Path,
    component: &str,
) -> bool {
    let mut dirs = runtime_dirs(library_dir);
    dirs.push(runtime_cache_dir());
    dirs.into_iter()
        .any(|dir| component_runtime_executable_present(&dir, component))
}

pub fn runtime_executable_ready_without_probe(java_exe: &Path) -> bool {
    runtime_executable_ready(java_exe)
}

pub fn managed_runtime_contents_verified_without_probe(runtime_root: &Path) -> bool {
    runtime_executable_ready(&java_executable(runtime_root))
        && persisted_runtime_manifest_verified(runtime_root)
}

pub fn find_java_runtime(
    library_dir: &Path,
    java_version: &JavaVersion,
    override_path: &str,
) -> Result<JavaRuntimeResult, JavaRuntimeLookupError> {
    let requirement = runtime_requirement(java_version);
    let runtime_override = parse_runtime_override(override_path);
    let record = match runtime_override {
        RuntimeOverride::None => {
            resolve_managed_runtime(library_dir, &requirement.preferred_component)?
        }
        RuntimeOverride::Component(component) => {
            resolve_component_runtime(library_dir, &component, java_version.major_version)?
        }
        RuntimeOverride::ExecutablePath(path) => {
            resolve_override_runtime(&path, &requirement.preferred_component, None)?.record
        }
    };

    Ok(JavaRuntimeResult {
        path: record.java_path,
        component: record.id.0,
        source: record.source.as_str().to_string(),
    })
}
pub fn preferred_runtime_component(java_version: &JavaVersion) -> String {
    if java_version.component.trim().is_empty() {
        java_component_for_major(java_version.major_version)
            .unwrap_or("java-runtime-delta")
            .to_string()
    } else {
        java_version.component.trim().to_string()
    }
}

pub fn is_known_runtime_component(value: &str) -> bool {
    known_runtime_components()
        .iter()
        .any(|component| *component == value.trim())
}

fn known_runtime_components() -> [&'static str; 6] {
    [
        "java-runtime-epsilon",
        "java-runtime-delta",
        "java-runtime-gamma",
        "java-runtime-beta",
        "java-runtime-alpha",
        "jre-legacy",
    ]
}

pub(super) fn resolve_component_runtime(
    library_dir: &Path,
    component: &RuntimeId,
    required_major: i32,
) -> Result<RuntimeRecord, JavaRuntimeLookupError> {
    let mut dirs = runtime_dirs(library_dir);
    dirs.push(runtime_cache_dir());
    resolve_component_runtime_from_roots(dirs, component, required_major, |dir| {
        inspect_component_runtime_for_resolution(dir, component.as_str())
    })
}

pub(super) fn resolve_component_runtime_from_roots(
    dirs: Vec<PathBuf>,
    component: &RuntimeId,
    required_major: i32,
    mut inspect: impl FnMut(&Path) -> Result<Option<RuntimeRecord>, JavaRuntimeLookupError>,
) -> Result<RuntimeRecord, JavaRuntimeLookupError> {
    // defer Rosetta blocks: a later root may hold a compatible runtime, and
    // surfacing beats NotFound since reinstall yields the same x86_64 build
    let mut rosetta_block = None;
    for dir in dirs {
        match inspect(&dir) {
            Ok(Some(record)) if record.install_state == RuntimeInstallState::Ready => {
                return Ok(record);
            }
            Ok(_) => {}
            Err(error @ JavaRuntimeLookupError::RosettaRequired { .. }) => {
                rosetta_block.get_or_insert(error);
            }
            Err(error) => return Err(error),
        }
    }

    Err(rosetta_block.unwrap_or(JavaRuntimeLookupError::NotFound {
        component: component.0.clone(),
        major: required_major,
    }))
}

pub(super) fn component_runtime_ready_without_probe(base_dir: &Path, component: &str) -> bool {
    if !runtime_filesystem_path(base_dir).as_ref().exists() {
        return false;
    }

    let os_arch = runtime_os_arch();
    [
        base_dir.join(component).join(&os_arch).join(component),
        base_dir.join(component),
    ]
    .into_iter()
    .any(|candidate| {
        detect_runtime_state(&candidate, runtime_requires_ready_marker(base_dir))
            == RuntimeInstallState::Ready
    })
}

fn component_runtime_executable_present(base_dir: &Path, component: &str) -> bool {
    if !runtime_filesystem_path(base_dir).as_ref().exists() {
        return false;
    }

    let os_arch = runtime_os_arch();
    [
        base_dir.join(component).join(&os_arch).join(component),
        base_dir.join(component),
    ]
    .into_iter()
    .any(|candidate| {
        let java = java_executable(&candidate);
        runtime_filesystem_path(&java).as_ref().is_file()
    })
}

pub(super) fn resolve_managed_runtime(
    library_dir: &Path,
    component: &RuntimeId,
) -> Result<RuntimeRecord, JavaRuntimeLookupError> {
    resolve_component_runtime(library_dir, component, 0)
}

pub(super) struct ResolvedOverrideRuntime {
    pub(super) record: RuntimeRecord,
    pub(super) probe_usage: super::model::RuntimeProbeUsage,
}

pub(super) fn resolve_override_runtime(
    path: &Path,
    preferred_component: &RuntimeId,
    receipt: Option<JavaRuntimeProbeReceipt>,
) -> Result<ResolvedOverrideRuntime, JavaRuntimeLookupError> {
    if !runtime_filesystem_path(path).as_ref().is_file() {
        return Err(JavaRuntimeLookupError::NotFound {
            component: "external-java-override".to_string(),
            major: 0,
        });
    }

    let receipt_supplied = receipt.is_some();
    let (info, probe_usage) = match receipt {
        Some(receipt) if receipt.matches_path(path).unwrap_or(false) => (
            receipt.into_info(),
            super::model::RuntimeProbeUsage {
                spawn_count: 0,
                source: super::model::RuntimeProbeSource::Receipt,
            },
        ),
        _ => {
            let receipt = probe_java_runtime_receipt(path, Some(preferred_component.as_str()))?;
            (
                receipt.into_info(),
                super::model::RuntimeProbeUsage {
                    spawn_count: 1,
                    source: if receipt_supplied {
                        super::model::RuntimeProbeSource::FreshAfterReceiptMismatch
                    } else {
                        super::model::RuntimeProbeSource::Fresh
                    },
                },
            )
        }
    };
    let canonical_path = PathBuf::from(&info.path);
    Ok(ResolvedOverrideRuntime {
        record: RuntimeRecord {
            id: preferred_component.clone(),
            java_path: info.path.clone(),
            info,
            source: RuntimeSource::ExternalOverride,
            install_state: RuntimeInstallState::Ready,
            root_dir: canonical_path
                .parent()
                .and_then(Path::parent)
                .unwrap_or_else(|| Path::new(""))
                .to_string_lossy()
                .to_string(),
        },
        probe_usage,
    })
}
pub(super) fn inspect_component_runtime(base_dir: &Path, component: &str) -> Option<RuntimeRecord> {
    inspect_component_runtime_checked(base_dir, component, false)
        .ok()
        .flatten()
}

fn inspect_component_runtime_for_resolution(
    base_dir: &Path,
    component: &str,
) -> Result<Option<RuntimeRecord>, JavaRuntimeLookupError> {
    inspect_component_runtime_checked(base_dir, component, true)
}

fn inspect_component_runtime_checked(
    base_dir: &Path,
    component: &str,
    strict_compatibility: bool,
) -> Result<Option<RuntimeRecord>, JavaRuntimeLookupError> {
    if !runtime_filesystem_path(base_dir).as_ref().exists() {
        return Ok(None);
    }

    let os_arch = runtime_os_arch();
    for candidate in [
        base_dir.join(component).join(&os_arch).join(component),
        base_dir.join(component),
    ] {
        let state = detect_runtime_state(&candidate, runtime_requires_ready_marker(base_dir));
        if state == RuntimeInstallState::Missing {
            continue;
        }

        let java_exe = java_executable(&candidate);
        let rosetta_required = if state == RuntimeInstallState::Ready {
            rosetta_required_error_for_current_host(&java_exe, component)
        } else {
            None
        };
        if rosetta_required.is_some() && strict_compatibility {
            return Err(JavaRuntimeLookupError::RosettaRequired {
                component: component.to_string(),
            });
        }
        let source = classify_runtime_source(base_dir);
        let info = if state == RuntimeInstallState::Ready && rosetta_required.is_none() {
            probe_java_runtime_info(&java_exe, Some(component)).unwrap_or(JavaRuntimeInfo {
                id: component.to_string(),
                major: 0,
                update: 0,
                distribution: "unknown".to_string(),
                path: java_exe.to_string_lossy().to_string(),
            })
        } else {
            JavaRuntimeInfo {
                id: component.to_string(),
                major: 0,
                update: 0,
                distribution: "unknown".to_string(),
                path: java_exe.to_string_lossy().to_string(),
            }
        };

        return Ok(Some(RuntimeRecord {
            id: RuntimeId(component.to_string()),
            java_path: java_exe.to_string_lossy().to_string(),
            info,
            source,
            install_state: state,
            root_dir: candidate.to_string_lossy().to_string(),
        }));
    }

    Ok(None)
}

pub(super) fn runtime_requires_ready_marker(base_dir: &Path) -> bool {
    base_dir == runtime_cache_dir()
}

pub(super) fn classify_runtime_source(base_dir: &Path) -> RuntimeSource {
    let label = base_dir.to_string_lossy();
    if label.contains("Packages") {
        RuntimeSource::MicrosoftStore
    } else if label.contains("axial") {
        RuntimeSource::Managed
    } else {
        RuntimeSource::MinecraftBundled
    }
}

pub(super) fn detect_runtime_state(
    runtime_root: &Path,
    require_ready_marker: bool,
) -> RuntimeInstallState {
    let installing_marker = runtime_root.join(".axial-installing");
    let ready_marker = runtime_root.join(".axial-ready");
    let java_exe = java_executable(runtime_root);

    if require_ready_marker {
        if runtime_filesystem_path(&installing_marker)
            .as_ref()
            .exists()
        {
            return RuntimeInstallState::Installing;
        }
        if runtime_filesystem_path(&ready_marker).as_ref().is_file()
            && managed_runtime_contents_verified_without_probe(runtime_root)
        {
            return RuntimeInstallState::Ready;
        }
        if runtime_filesystem_path(&ready_marker).as_ref().exists()
            || runtime_filesystem_path(runtime_root).as_ref().exists()
        {
            return RuntimeInstallState::Broken;
        }
        return RuntimeInstallState::Missing;
    }

    if runtime_executable_ready(&java_exe) {
        return RuntimeInstallState::Ready;
    }
    if runtime_filesystem_path(&installing_marker)
        .as_ref()
        .exists()
    {
        return RuntimeInstallState::Installing;
    }
    if runtime_filesystem_path(&ready_marker).as_ref().exists()
        || runtime_filesystem_path(runtime_root).as_ref().exists()
    {
        return RuntimeInstallState::Broken;
    }
    RuntimeInstallState::Missing
}

fn persisted_runtime_manifest_verified(runtime_root: &Path) -> bool {
    let manifest_path = runtime_root.join(COMPONENT_MANIFEST_PROOF_FILE);
    let Ok(data) = std::fs::read(runtime_filesystem_path(&manifest_path).as_ref()) else {
        return false;
    };
    let Ok(manifest) = serde_json::from_slice::<ComponentManifest>(&data) else {
        return false;
    };

    let mut file_jobs = Vec::new();
    let mut link_jobs = Vec::new();
    let mut saw_file = false;
    for (relative_path, file) in manifest.files {
        let Ok(path) = component_manifest_destination(runtime_root, &relative_path) else {
            return false;
        };
        match file.kind.as_str() {
            "directory" => {
                if !runtime_filesystem_path(&path).as_ref().is_dir() {
                    return false;
                }
            }
            "file" => {
                let Some(raw) = file.downloads.and_then(|downloads| downloads.raw) else {
                    return false;
                };
                let Some(expected_sha1) = raw.sha1.as_deref() else {
                    return false;
                };
                if !runtime_sha1_hex(expected_sha1) {
                    return false;
                }
                saw_file = true;
                file_jobs.push(RuntimeVerificationJob {
                    relative_path,
                    path,
                    expected: RuntimeDownloadEvidence {
                        size: raw.size,
                        sha1: raw.sha1,
                    },
                });
            }
            "link" => {
                let Some(target) = file.target else {
                    return false;
                };
                let Ok(target_path) = component_manifest_link_target_path(
                    runtime_root,
                    &path,
                    &relative_path,
                    &target,
                ) else {
                    return false;
                };
                link_jobs.push(RuntimeLinkVerificationJob {
                    path,
                    target,
                    target_path,
                });
            }
            _ => return false,
        }
    }

    saw_file && verify_runtime_jobs(file_jobs) && link_jobs.into_iter().all(verify_runtime_link_job)
}

#[derive(Clone)]
struct RuntimeVerificationJob {
    relative_path: String,
    path: PathBuf,
    expected: RuntimeDownloadEvidence,
}

fn verify_runtime_jobs(jobs: Vec<RuntimeVerificationJob>) -> bool {
    let worker_count = available_runtime_parallelism()
        .saturating_mul(2)
        .clamp(2, 16)
        .min(jobs.len());
    if worker_count <= 1 {
        return jobs.into_iter().all(verify_runtime_job);
    }

    let chunk_size = jobs.len().div_ceil(worker_count);
    let handles = jobs
        .chunks(chunk_size)
        .map(|chunk| {
            let chunk = chunk.to_vec();
            std::thread::spawn(move || chunk.into_iter().all(verify_runtime_job))
        })
        .collect::<Vec<_>>();

    handles
        .into_iter()
        .all(|handle| handle.join().unwrap_or(false))
}

fn verify_runtime_job(job: RuntimeVerificationJob) -> bool {
    let Ok(actual) = runtime_file_actual(&job.path) else {
        return false;
    };
    verify_runtime_download(&job.relative_path, &job.expected, &actual).is_ok()
}

struct RuntimeLinkVerificationJob {
    path: PathBuf,
    target: String,
    target_path: PathBuf,
}

#[cfg(unix)]
fn verify_runtime_link_job(job: RuntimeLinkVerificationJob) -> bool {
    let Ok(metadata) = std::fs::symlink_metadata(runtime_filesystem_path(&job.path).as_ref())
    else {
        return false;
    };
    if !metadata.file_type().is_symlink() {
        return false;
    }
    let Ok(actual_target) = std::fs::read_link(runtime_filesystem_path(&job.path).as_ref()) else {
        return false;
    };
    actual_target == Path::new(&job.target)
        && runtime_filesystem_path(&job.target_path).as_ref().exists()
}

#[cfg(not(unix))]
fn verify_runtime_link_job(_job: RuntimeLinkVerificationJob) -> bool {
    false
}

fn runtime_sha1_hex(value: &str) -> bool {
    value.len() == 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn runtime_file_actual(path: &Path) -> std::io::Result<RuntimeDownloadActual> {
    let mut file = std::fs::File::open(runtime_filesystem_path(path).as_ref())?;
    let mut hasher = Sha1::new();
    let mut size = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        size += read as u64;
    }
    Ok(RuntimeDownloadActual {
        size,
        sha1: format!("{:x}", hasher.finalize()),
    })
}
