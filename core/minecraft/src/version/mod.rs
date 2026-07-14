use crate::launch::{Downloads, JavaVersion, effective_java_version_for};
use crate::loaders::{MaterializedLoaderProfile, validate_materialized_loader_profile};
use crate::managed_fs::{ManagedDir, ManagedDirectoryIdentity};
use crate::managed_publication::{ManagedPublicationError, ManagedRootPublicationReadLease};
use crate::paths::versions_dir;
use crate::types::{VersionEntry, VersionLoaderAttachment, VersionSubjectKind};
use crate::version_meta::{analyze_minecraft_version, compare_version_entries};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::Path;
use std::time::SystemTime;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionScanReport {
    pub state: VersionScanState,
    pub versions: Vec<VersionEntry>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub issues: Vec<VersionScanIssue>,
}

pub struct VersionScanSnapshot {
    pub report: VersionScanReport,
    dependencies: VersionScanDependencyStamp,
}

pub struct VersionBundleReadGuard {
    admission: VersionBundleReadAdmission,
}

enum VersionBundleReadAdmission {
    MissingRoot(std::path::PathBuf),
    Managed(ManagedRootPublicationReadLease),
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum VersionBundleRootBinding {
    Missing,
    Managed(ManagedDirectoryIdentity),
}

impl VersionBundleReadGuard {
    pub fn acquire(library_dir: &Path) -> io::Result<Self> {
        let root = match ManagedDir::open_root(library_dir) {
            Ok(root) => root,
            Err(crate::loaders::types::LoaderError::Io(error))
                if error.kind() == io::ErrorKind::NotFound =>
            {
                return Ok(Self {
                    admission: VersionBundleReadAdmission::MissingRoot(library_dir.to_path_buf()),
                });
            }
            Err(error) => return Err(io::Error::other(error)),
        };
        let lease =
            ManagedRootPublicationReadLease::acquire(root).map_err(publication_read_error)?;
        Ok(Self {
            admission: VersionBundleReadAdmission::Managed(lease),
        })
    }

    pub fn revalidate(&self) -> io::Result<()> {
        match &self.admission {
            VersionBundleReadAdmission::MissingRoot(path) => match fs::symlink_metadata(path) {
                Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
                Ok(_) => Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "managed publication root appeared during read",
                )),
                Err(error) => Err(error),
            },
            VersionBundleReadAdmission::Managed(lease) => {
                lease.revalidate().map_err(publication_read_error)
            }
        }
    }

    fn root_binding(&self) -> io::Result<VersionBundleRootBinding> {
        match &self.admission {
            VersionBundleReadAdmission::MissingRoot(_) => Ok(VersionBundleRootBinding::Missing),
            VersionBundleReadAdmission::Managed(lease) => lease
                .root_identity()
                .map(VersionBundleRootBinding::Managed)
                .map_err(publication_read_error),
        }
    }
}

impl VersionScanSnapshot {
    pub fn dependencies(&self) -> &VersionScanDependencyStamp {
        &self.dependencies
    }
}

#[derive(Clone)]
pub struct VersionScanDependencyStamp {
    library_dir: std::path::PathBuf,
    root_binding: VersionBundleRootBinding,
    observations: Vec<DependencyObservation>,
    revalidatable: bool,
}

impl VersionScanDependencyStamp {
    pub fn is_revalidated(&self) -> bool {
        let Ok(publication_read) = VersionBundleReadGuard::acquire(&self.library_dir) else {
            return false;
        };
        let Ok(root_binding) = publication_read.root_binding() else {
            return false;
        };
        self.root_binding == root_binding
            && self.revalidatable
            && self
                .observations
                .iter()
                .all(DependencyObservation::is_revalidated)
            && publication_read.revalidate().is_ok()
    }
}

#[derive(Clone, Eq, PartialEq)]
struct DependencyObservation {
    path: std::path::PathBuf,
    link: DependencyMetadata,
    target: Option<DependencyMetadata>,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum DependencyMetadata {
    Missing,
    Present {
        kind: DependencyKind,
        modified: SystemTime,
        len: u64,
        readonly: bool,
        mode: u32,
    },
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum DependencyKind {
    File,
    Directory,
    Symlink,
    Other,
}

#[derive(Default)]
struct DependencyTracker {
    observations: Vec<DependencyObservation>,
    revalidatable: bool,
}

impl DependencyTracker {
    fn new() -> Self {
        Self {
            observations: Vec::new(),
            revalidatable: true,
        }
    }

    fn begin(&mut self, path: &Path) -> Option<DependencyObservation> {
        let observation = DependencyObservation::capture(path);
        if observation.is_none() {
            self.revalidatable = false;
        }
        observation
    }

    fn finish(&mut self, path: &Path, before: Option<DependencyObservation>) {
        let after = DependencyObservation::capture(path);
        match (before, after) {
            (Some(before), Some(after)) if before == after => self.observations.push(after),
            (Some(before), Some(after)) => {
                self.revalidatable = false;
                self.observations.extend([before, after]);
            }
            (Some(before), None) => {
                self.revalidatable = false;
                self.observations.push(before);
            }
            (None, Some(after)) => {
                self.revalidatable = false;
                self.observations.push(after);
            }
            (None, None) => self.revalidatable = false,
        }
    }

    fn read_to_string(&mut self, path: &Path) -> io::Result<String> {
        let before = self.begin(path);
        let result = fs::read_to_string(path);
        self.finish(path, before);
        result
    }

    fn is_dir(&mut self, path: &Path) -> bool {
        self.observe_kind(path) == Some(DependencyKind::Directory)
    }

    fn is_file(&mut self, path: &Path) -> bool {
        self.observe_kind(path) == Some(DependencyKind::File)
    }

    fn mark_unrevalidatable(&mut self) {
        self.revalidatable = false;
    }

    fn observe_kind(&mut self, path: &Path) -> Option<DependencyKind> {
        match DependencyObservation::capture(path) {
            Some(observation) => {
                let kind = observation.effective_kind();
                self.observations.push(observation);
                kind
            }
            None => {
                self.revalidatable = false;
                None
            }
        }
    }

    fn into_stamp(
        self,
        library_dir: &Path,
        root_binding: VersionBundleRootBinding,
    ) -> VersionScanDependencyStamp {
        VersionScanDependencyStamp {
            library_dir: library_dir.to_path_buf(),
            root_binding,
            observations: self.observations,
            revalidatable: self.revalidatable,
        }
    }
}

impl DependencyObservation {
    fn capture(path: &Path) -> Option<Self> {
        let link = capture_dependency_metadata(path, false)?;
        let target = if matches!(
            link,
            DependencyMetadata::Present {
                kind: DependencyKind::Symlink,
                ..
            }
        ) {
            Some(capture_dependency_metadata(path, true)?)
        } else {
            None
        };
        Some(Self {
            path: path.to_path_buf(),
            link,
            target,
        })
    }

    fn is_revalidated(&self) -> bool {
        Self::capture(&self.path).is_some_and(|current| current == *self)
    }

    fn effective_kind(&self) -> Option<DependencyKind> {
        match self.target.unwrap_or(self.link) {
            DependencyMetadata::Missing => None,
            DependencyMetadata::Present { kind, .. } => Some(kind),
        }
    }
}

fn capture_dependency_metadata(path: &Path, follow: bool) -> Option<DependencyMetadata> {
    let result = if follow {
        fs::metadata(path)
    } else {
        fs::symlink_metadata(path)
    };
    match result {
        Ok(metadata) => Some(DependencyMetadata::Present {
            kind: if !follow && metadata.file_type().is_symlink() {
                DependencyKind::Symlink
            } else if metadata.is_file() {
                DependencyKind::File
            } else if metadata.is_dir() {
                DependencyKind::Directory
            } else {
                DependencyKind::Other
            },
            modified: metadata.modified().ok()?,
            len: metadata.len(),
            readonly: metadata.permissions().readonly(),
            mode: dependency_mode(&metadata),
        }),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Some(DependencyMetadata::Missing),
        Err(_) => None,
    }
}

#[cfg(unix)]
fn dependency_mode(metadata: &fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode()
}

#[cfg(not(unix))]
fn dependency_mode(_metadata: &fs::Metadata) -> u32 {
    0
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VersionScanState {
    Ready,
    Empty,
    Degraded,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionScanIssue {
    pub kind: VersionScanIssueKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VersionScanIssueKind {
    VersionsDirectoryUnreadable,
    VersionDirectoryEntryUnreadable,
    VersionJsonMissing,
    VersionJsonUnreadable,
    VersionJsonMalformed,
    LoaderIdentityMalformed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedVersion {
    pub id: String,
    #[serde(default)]
    pub inherits_from: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct VersionStub {
    #[serde(default)]
    id: String,
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(rename = "releaseTime", default)]
    release_time: String,
    #[serde(rename = "inheritsFrom", default)]
    inherits_from: String,
    #[serde(rename = "axialMaterialized", default)]
    materialized: bool,
    #[serde(rename = "javaVersion", default)]
    java_version: Option<JavaVersion>,
    #[serde(default)]
    downloads: Downloads,
}

pub fn scan_versions(mc_dir: &Path) -> io::Result<Vec<VersionEntry>> {
    scan_versions_report(mc_dir).map(|report| report.versions)
}

pub fn scan_versions_report(mc_dir: &Path) -> io::Result<VersionScanReport> {
    scan_versions_snapshot(mc_dir).map(|snapshot| snapshot.report)
}

pub fn scan_versions_snapshot(mc_dir: &Path) -> io::Result<VersionScanSnapshot> {
    let publication_read = VersionBundleReadGuard::acquire(mc_dir)?;
    let versions_dir = versions_dir(mc_dir);
    let mut dependencies = DependencyTracker::new();
    let versions_root_before = dependencies.begin(&versions_dir);
    let entries = match fs::read_dir(&versions_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            dependencies.finish(&versions_dir, versions_root_before);
            return finish_scan_snapshot(
                VersionScanReport {
                    state: VersionScanState::Empty,
                    versions: Vec::new(),
                    issues: Vec::new(),
                },
                dependencies,
                mc_dir,
                &publication_read,
            );
        }
        Err(_) => {
            dependencies.finish(&versions_dir, versions_root_before);
            return finish_scan_snapshot(
                VersionScanReport {
                    state: VersionScanState::Degraded,
                    versions: Vec::new(),
                    issues: vec![version_scan_issue(
                        VersionScanIssueKind::VersionsDirectoryUnreadable,
                        None,
                    )],
                },
                dependencies,
                mc_dir,
                &publication_read,
            );
        }
    };
    let mut stubs = HashMap::new();
    let mut loader_profiles = HashMap::new();
    let mut issues = Vec::new();

    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => {
                dependencies.mark_unrevalidatable();
                issues.push(version_scan_issue(
                    VersionScanIssueKind::VersionDirectoryEntryUnreadable,
                    None,
                ));
                continue;
            }
        };
        let entry_path = entry.path();
        let is_dir = dependencies.is_dir(&entry_path);
        if !is_dir {
            continue;
        }
        let id = entry.file_name().to_string_lossy().to_string();
        let reserved_loader_id = crate::loaders::api::is_reserved_installed_loader_id(&id);
        let json_path = entry_path.join(format!("{id}.json"));
        let data_result = dependencies.read_to_string(&json_path);
        let data = match data_result {
            Ok(data) => data,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                issues.push(version_scan_issue(
                    VersionScanIssueKind::VersionJsonMissing,
                    Some(id),
                ));
                continue;
            }
            Err(_) => {
                issues.push(version_scan_issue(
                    VersionScanIssueKind::VersionJsonUnreadable,
                    Some(id),
                ));
                continue;
            }
        };
        let stub = match serde_json::from_str::<VersionStub>(&data) {
            Ok(stub) => stub,
            Err(_) => {
                issues.push(version_scan_issue(
                    VersionScanIssueKind::VersionJsonMalformed,
                    Some(id),
                ));
                continue;
            }
        };
        match validate_materialized_loader_profile(
            &id,
            &stub.id,
            &stub.inherits_from,
            stub.materialized,
        ) {
            Ok(profile) => {
                loader_profiles.insert(id.clone(), profile);
            }
            Err(_) if stub.materialized || reserved_loader_id => {
                issues.push(version_scan_issue(
                    VersionScanIssueKind::LoaderIdentityMalformed,
                    Some(id),
                ));
                continue;
            }
            Err(_) => {}
        }
        stubs.insert(id, stub);
    }

    let mut versions = Vec::new();
    for (id, stub) in &stubs {
        let loader_profile = loader_profiles.get(id);
        let effective_parent = stub.inherits_from.clone();
        let jar_path = versions_dir.join(id).join(format!("{id}.jar"));

        let resolved_java = resolve_java_version(id, &stubs);
        let (launchable, status, status_detail, needs_install) = if effective_parent.is_empty() {
            let jar_ready = dependencies.is_file(&jar_path);
            if jar_ready {
                (true, "ready".to_string(), String::new(), String::new())
            } else {
                (
                    false,
                    "incomplete".to_string(),
                    "Game files not fully downloaded".to_string(),
                    id.clone(),
                )
            }
        } else {
            let parent_json = versions_dir
                .join(&effective_parent)
                .join(format!("{}.json", effective_parent));
            let parent_jar = versions_dir
                .join(&effective_parent)
                .join(format!("{}.jar", effective_parent));
            let child_has_client_artifact = stub.downloads.client.is_some();
            let parent_json_ready = dependencies.is_file(&parent_json);
            if !parent_json_ready {
                (
                    false,
                    "incomplete".to_string(),
                    format!("Base version {} needs to be installed", effective_parent),
                    effective_parent.clone(),
                )
            } else {
                let jar_ready = dependencies.is_file(&jar_path);
                if child_has_client_artifact && !jar_ready {
                    (
                        false,
                        "incomplete".to_string(),
                        "Game files not fully downloaded".to_string(),
                        id.clone(),
                    )
                } else if jar_ready {
                    (true, "ready".to_string(), String::new(), String::new())
                } else {
                    let parent_jar_ready = dependencies.is_file(&parent_jar);
                    if !parent_jar_ready {
                        (
                            false,
                            "incomplete".to_string(),
                            format!("Base version {} needs to be downloaded", effective_parent),
                            effective_parent.clone(),
                        )
                    } else {
                        (true, "ready".to_string(), String::new(), String::new())
                    }
                }
            }
        };

        let analysis_id = if effective_parent.is_empty() {
            id.as_str()
        } else {
            effective_parent.as_str()
        };
        let analysis_stub = stubs.get(analysis_id).unwrap_or(stub);
        let analysis = analyze_minecraft_version(
            analysis_id,
            &analysis_stub.kind,
            &analysis_stub.release_time,
            None,
            &[],
        );
        let loader = loader_profile.map(loader_attachment_from_profile);

        versions.push(VersionEntry {
            subject_kind: VersionSubjectKind::InstalledVersion,
            id: id.clone(),
            raw_kind: analysis_stub.kind.clone(),
            release_time: analysis_stub.release_time.clone(),
            minecraft_meta: analysis.minecraft_meta,
            lifecycle: analysis.lifecycle,
            inherits_from: effective_parent.clone(),
            launchable,
            installed: true,
            status,
            status_detail,
            needs_install,
            java_component: resolved_java.component,
            java_major: resolved_java.major_version,
            loader,
        });
    }

    versions.sort_by(compare_version_entries);
    let state = if !issues.is_empty() {
        VersionScanState::Degraded
    } else if versions.is_empty() {
        VersionScanState::Empty
    } else {
        VersionScanState::Ready
    };
    dependencies.finish(&versions_dir, versions_root_before);
    finish_scan_snapshot(
        VersionScanReport {
            state,
            versions,
            issues,
        },
        dependencies,
        mc_dir,
        &publication_read,
    )
}

fn finish_scan_snapshot(
    report: VersionScanReport,
    dependencies: DependencyTracker,
    library_dir: &Path,
    publication_read: &VersionBundleReadGuard,
) -> io::Result<VersionScanSnapshot> {
    publication_read.revalidate()?;
    let root_binding = publication_read.root_binding()?;
    Ok(VersionScanSnapshot {
        report,
        dependencies: dependencies.into_stamp(library_dir, root_binding),
    })
}

fn publication_read_error(error: ManagedPublicationError) -> io::Error {
    match error {
        ManagedPublicationError::ReadBusy => io::Error::new(io::ErrorKind::WouldBlock, error),
        error => io::Error::other(error),
    }
}

fn resolve_java_version(id: &str, stubs: &HashMap<String, VersionStub>) -> JavaVersion {
    let mut current_id = id.to_string();
    let mut current = stubs.get(&current_id);
    let mut fallback_parent = String::new();
    while let Some(stub) = current {
        if let Some(java_version) = &stub.java_version {
            return effective_java_version_for(&current_id, &stub.kind, java_version);
        }
        let next_parent = stub.inherits_from.clone();
        if next_parent.is_empty() {
            break;
        }
        if next_parent == current_id {
            break;
        }
        fallback_parent = next_parent.clone();
        current_id = next_parent.clone();
        current = stubs.get(&next_parent);
    }

    if !fallback_parent.is_empty()
        && fallback_parent != id
        && let Some(stub) = stubs.get(&fallback_parent)
        && let Some(java_version) = &stub.java_version
    {
        return effective_java_version_for(&fallback_parent, &stub.kind, java_version);
    }

    let inference_id = if fallback_parent.is_empty() {
        id
    } else {
        fallback_parent.as_str()
    };
    let raw_kind = stubs
        .get(inference_id)
        .or_else(|| stubs.get(id))
        .map(|stub| stub.kind.as_str())
        .unwrap_or_default();
    effective_java_version_for(inference_id, raw_kind, &JavaVersion::default())
}

fn loader_attachment_from_profile(profile: &MaterializedLoaderProfile) -> VersionLoaderAttachment {
    VersionLoaderAttachment {
        component_id: profile.component_id(),
        component_name: profile.component_id().display_name().to_string(),
        build_id: crate::loaders::build_id_for(
            profile.component_id(),
            profile.minecraft_version(),
            profile.loader_version(),
        ),
        loader_version: profile.loader_version().to_string(),
        build_meta: profile.display_metadata(),
    }
}

fn version_scan_issue(kind: VersionScanIssueKind, version_id: Option<String>) -> VersionScanIssue {
    VersionScanIssue { kind, version_id }
}

#[cfg(test)]
mod tests {
    use super::{
        VersionScanIssueKind, VersionScanState, VersionStub, resolve_java_version, scan_versions,
        scan_versions_report,
    };
    use crate::launch::{Downloads, JavaVersion};
    use crate::loaders::installed_version_id_for;
    use crate::loaders::types::{LoaderComponentId, LoaderSelectionReason, LoaderTerm};
    use std::collections::HashMap;
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn resolve_java_version_follows_declared_parent_chain_for_loader_versions() {
        let loader_id = installed_version_id_for(LoaderComponentId::Fabric, "1.20.1", "0.14.21")
            .expect("valid loader identity");
        let mut stubs = HashMap::new();
        stubs.insert(
            loader_id.clone(),
            VersionStub {
                id: loader_id.clone(),
                kind: "release".to_string(),
                release_time: String::new(),
                inherits_from: "1.20.1".to_string(),
                materialized: true,
                java_version: None,
                downloads: Downloads::default(),
            },
        );
        stubs.insert(
            "1.20.1".to_string(),
            VersionStub {
                id: "1.20.1".to_string(),
                kind: "release".to_string(),
                release_time: String::new(),
                inherits_from: String::new(),
                materialized: false,
                java_version: Some(JavaVersion {
                    component: "java-runtime-gamma".to_string(),
                    major_version: 17,
                }),
                downloads: Downloads::default(),
            },
        );
        let resolved = resolve_java_version(&loader_id, &stubs);

        assert_eq!(resolved.component, "java-runtime-gamma");
        assert_eq!(resolved.major_version, 17);
    }

    #[test]
    fn scan_versions_marks_missing_parent_as_install_target() {
        let mc_dir = unique_test_dir("missing-parent-install-target");
        let versions_dir = mc_dir.join("versions");
        let child_id = "custom-child";
        let child_dir = versions_dir.join(child_id);
        fs::create_dir_all(&child_dir).expect("create child version dir");
        fs::write(
            child_dir.join(format!("{child_id}.json")),
            r#"{
                "id":"custom-child",
                "inheritsFrom":"1.20.1",
                "type":"release"
            }"#,
        )
        .expect("write child json");

        let versions = scan_versions(&mc_dir).expect("scan versions");
        let version = versions
            .iter()
            .find(|entry| entry.id == child_id)
            .expect("child version exists");

        assert_eq!(version.status, "incomplete");
        assert_eq!(version.needs_install, "1.20.1");
        assert!(version.status_detail.contains("1.20.1"));

        let _ = fs::remove_dir_all(&mc_dir);
    }

    #[test]
    fn scan_versions_report_distinguishes_empty_library_from_degraded_scan() {
        let mc_dir = unique_test_dir("empty-library-scan");

        let report = scan_versions_report(&mc_dir).expect("scan empty library");

        assert_eq!(report.state, VersionScanState::Empty);
        assert!(report.versions.is_empty());
        assert!(report.issues.is_empty());

        let _ = fs::remove_dir_all(&mc_dir);
    }

    #[test]
    fn missing_library_admission_is_invalidated_when_the_root_appears() {
        let mc_dir = unique_test_dir("appearing-library-root");
        let read =
            super::VersionBundleReadGuard::acquire(&mc_dir).expect("admit missing library root");

        fs::create_dir_all(&mc_dir).expect("create library root");

        assert_eq!(
            read.revalidate()
                .expect_err("appearing root must invalidate read")
                .kind(),
            std::io::ErrorKind::WouldBlock
        );
        fs::remove_dir_all(&mc_dir).expect("remove temp test dir");
    }

    #[test]
    fn dependency_stamp_rejects_equivalent_replacement_library_root() {
        let mc_dir = unique_test_dir("replacement-library-root");
        let displaced = mc_dir.with_extension("displaced");
        fs::create_dir_all(&mc_dir).expect("create original library root");
        let snapshot = super::scan_versions_snapshot(&mc_dir).expect("scan original root");

        fs::rename(&mc_dir, &displaced).expect("displace original library root");
        fs::create_dir_all(&mc_dir).expect("create equivalent replacement root");

        assert!(!snapshot.dependencies().is_revalidated());
        fs::remove_dir_all(&mc_dir).expect("remove replacement root");
        fs::remove_dir_all(&displaced).expect("remove displaced root");
    }

    #[test]
    fn scan_versions_report_marks_malformed_version_json_as_degraded() {
        let mc_dir = unique_test_dir("malformed-version-scan");
        let version_dir = mc_dir.join("versions").join("1.21.1");
        fs::create_dir_all(&version_dir).expect("create version dir");
        fs::write(version_dir.join("1.21.1.json"), "{not valid json")
            .expect("write malformed version json");

        let report = scan_versions_report(&mc_dir).expect("scan degraded library");

        assert_eq!(report.state, VersionScanState::Degraded);
        assert!(report.versions.is_empty());
        assert!(report.issues.iter().any(|issue| {
            issue.kind == VersionScanIssueKind::VersionJsonMalformed
                && issue.version_id.as_deref() == Some("1.21.1")
        }));

        fs::remove_dir_all(&mc_dir).expect("remove temp test dir");
    }

    #[test]
    fn scan_versions_reports_partial_reserved_loader_directory_normally() {
        let mc_dir = unique_test_dir("partial-reserved-loader");
        let loader_id = installed_version_id_for(LoaderComponentId::Quilt, "1.21.5", "0.29.2")
            .expect("valid loader identity");
        fs::create_dir_all(mc_dir.join("versions").join(&loader_id))
            .expect("create partial loader directory");

        let report = scan_versions_report(&mc_dir).expect("scan partial loader directory");

        assert_eq!(report.state, VersionScanState::Degraded);
        assert!(report.versions.is_empty());
        assert!(report.issues.iter().any(|issue| {
            issue.kind == VersionScanIssueKind::VersionJsonMissing
                && issue.version_id.as_deref() == Some(loader_id.as_str())
        }));

        fs::remove_dir_all(&mc_dir).expect("remove temp test dir");
    }

    #[tokio::test]
    async fn scan_versions_returns_transient_error_while_publication_is_exclusive() {
        let mc_dir = unique_test_dir("active-publication");
        fs::create_dir_all(&mc_dir).expect("create library root");
        let writer = crate::managed_publication::ManagedRootPublicationLease::acquire(
            crate::managed_fs::ManagedDir::open_root(&mc_dir).expect("managed root"),
        )
        .await
        .expect("writer admission");

        let error = scan_versions_report(&mc_dir).expect_err("active writer must deny scan");

        assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock);
        drop(writer);
        fs::remove_dir_all(&mc_dir).expect("remove temp test dir");
    }

    #[test]
    fn scan_versions_infers_java8_for_legacy_versions_without_java_version() {
        let mc_dir = unique_test_dir("legacy-java-scan");
        let versions_dir = mc_dir.join("versions");
        for version_id in ["1.8.9", "1.12.2"] {
            let version_dir = versions_dir.join(version_id);
            fs::create_dir_all(&version_dir).expect("version dir");
            fs::write(
                version_dir.join(format!("{version_id}.json")),
                format!(
                    r#"{{
                        "id":"{version_id}",
                        "type":"release",
                        "mainClass":"net.minecraft.client.main.Main",
                        "libraries":[]
                    }}"#
                ),
            )
            .expect("write version json");
            fs::write(version_dir.join(format!("{version_id}.jar")), b"client")
                .expect("write client jar");
        }

        let versions = scan_versions(&mc_dir).expect("scan versions");

        for version_id in ["1.8.9", "1.12.2"] {
            let version = versions
                .iter()
                .find(|entry| entry.id == version_id)
                .expect("version exists");
            assert_eq!(version.java_component, "jre-legacy");
            assert_eq!(version.java_major, 8);
        }

        fs::remove_dir_all(&mc_dir).expect("remove temp test dir");
    }

    #[test]
    fn scan_versions_derives_loader_lifecycle_from_canonical_profile() {
        let mc_dir = unique_test_dir("loader-lifecycle-profile");
        let versions_dir = mc_dir.join("versions");
        let version_id =
            installed_version_id_for(LoaderComponentId::Forge, "26.1.2", "64.0.4-beta")
                .expect("valid loader identity");
        let forge_dir = versions_dir.join(&version_id);
        fs::create_dir_all(&forge_dir).expect("create forge version dir");
        fs::write(
            forge_dir.join(format!("{version_id}.json")),
            format!(
                r#"{{
                "id":"{version_id}",
                "inheritsFrom":"26.1.2",
                "axialMaterialized":true,
                "type":"release"
            }}"#
            ),
        )
        .expect("write forge json");

        let versions = scan_versions(&mc_dir).expect("scan versions");
        let version = versions
            .iter()
            .find(|entry| entry.id == version_id)
            .expect("forge version exists");

        let loader = version.loader.as_ref().expect("loader lifecycle exists");
        assert_eq!(loader.component_id, LoaderComponentId::Forge);
        assert_eq!(loader.loader_version, "64.0.4-beta");
        assert!(loader.build_meta.terms.contains(&LoaderTerm::Beta));
        assert_eq!(
            loader.build_meta.selection.reason,
            LoaderSelectionReason::Unstable
        );
        assert_eq!(loader.build_meta.display_tags, vec!["beta"]);

        fs::remove_dir_all(&mc_dir).expect("remove temp test dir");
    }

    #[test]
    fn scan_versions_anchors_loader_profile_to_base_minecraft_version() {
        let mc_dir = unique_test_dir("loader-base-version-profile");
        let versions_dir = mc_dir.join("versions");
        let base_dir = versions_dir.join("1.21.5");
        fs::create_dir_all(&base_dir).expect("create base version dir");
        fs::write(
            base_dir.join("1.21.5.json"),
            r#"{
                "id":"1.21.5",
                "type":"release",
                "releaseTime":"2025-03-25T12:00:00+00:00",
                "mainClass":"net.minecraft.client.main.Main",
                "libraries":[]
            }"#,
        )
        .expect("write base json");
        fs::write(base_dir.join("1.21.5.jar"), b"client").expect("write base jar");

        let version_id = installed_version_id_for(LoaderComponentId::Fabric, "1.21.5", "0.19.3")
            .expect("valid loader identity");
        let fabric_dir = versions_dir.join(&version_id);
        fs::create_dir_all(&fabric_dir).expect("create fabric version dir");
        fs::write(
            fabric_dir.join(format!("{version_id}.json")),
            format!(
                r#"{{
                "id":"{version_id}",
                "inheritsFrom":"1.21.5",
                "axialMaterialized":true,
                "mainClass":"net.fabricmc.loader.impl.launch.knot.KnotClient",
                "libraries":[]
            }}"#
            ),
        )
        .expect("write fabric json");
        let versions = scan_versions(&mc_dir).expect("scan versions");
        let version = versions
            .iter()
            .find(|entry| entry.id == version_id)
            .expect("fabric version exists");

        assert_eq!(version.inherits_from, "1.21.5");
        assert_eq!(version.raw_kind, "release");
        assert_eq!(version.release_time, "2025-03-25T12:00:00+00:00");
        assert_eq!(version.minecraft_meta.display_name, "1.21.5");
        assert_eq!(version.minecraft_meta.effective_version, "1.21.5");

        fs::remove_dir_all(&mc_dir).expect("remove temp test dir");
    }

    #[test]
    fn scan_versions_rejects_loader_profile_with_a_different_declared_parent() {
        let mc_dir = unique_test_dir("loader-parent-mismatch");
        let version_id = installed_version_id_for(LoaderComponentId::NeoForge, "1.21.5", "21.5.75")
            .expect("valid loader identity");
        let version_dir = mc_dir.join("versions").join(&version_id);
        fs::create_dir_all(&version_dir).expect("create loader version dir");
        fs::write(
            version_dir.join(format!("{version_id}.json")),
            format!(
                r#"{{
                    "id":"{version_id}",
                    "inheritsFrom":"1.21.4",
                    "axialMaterialized":true,
                    "type":"release"
                }}"#
            ),
        )
        .expect("write loader json");
        let report = scan_versions_report(&mc_dir).expect("scan versions");
        assert!(report.versions.iter().all(|entry| entry.id != version_id));
        assert!(report.issues.iter().any(|issue| {
            issue.kind == VersionScanIssueKind::LoaderIdentityMalformed
                && issue.version_id.as_deref() == Some(version_id.as_str())
        }));

        fs::remove_dir_all(&mc_dir).expect("remove temp test dir");
    }

    #[test]
    fn scan_versions_rejects_canonical_loader_id_without_materialized_marker() {
        let mc_dir = unique_test_dir("loader-marker-missing");
        let version_id = installed_version_id_for(LoaderComponentId::Quilt, "1.21.5", "0.29.2")
            .expect("valid loader identity");
        let version_dir = mc_dir.join("versions").join(&version_id);
        fs::create_dir_all(&version_dir).expect("create loader version dir");
        fs::write(
            version_dir.join(format!("{version_id}.json")),
            format!(
                r#"{{
                    "id":"{version_id}",
                    "inheritsFrom":"1.21.5",
                    "type":"release"
                }}"#
            ),
        )
        .expect("write loader json");

        let report = scan_versions_report(&mc_dir).expect("scan versions");
        assert!(report.versions.iter().all(|entry| entry.id != version_id));
        assert!(report.issues.iter().any(|issue| {
            issue.kind == VersionScanIssueKind::LoaderIdentityMalformed
                && issue.version_id.as_deref() == Some(version_id.as_str())
        }));

        fs::remove_dir_all(mc_dir).expect("remove temp test dir");
    }

    #[test]
    fn scan_versions_rejects_malformed_reserved_loader_id_without_marker_or_parent() {
        let mc_dir = unique_test_dir("malformed-reserved-loader-id");
        let version_id = "loader-v2-malformed";
        let version_dir = mc_dir.join("versions").join(version_id);
        fs::create_dir_all(&version_dir).expect("create loader version dir");
        fs::write(
            version_dir.join(format!("{version_id}.json")),
            format!(r#"{{"id":"{version_id}","type":"release"}}"#),
        )
        .expect("write loader json");

        let report = scan_versions_report(&mc_dir).expect("scan versions");
        assert!(report.versions.iter().all(|entry| entry.id != version_id));
        assert!(report.issues.iter().any(|issue| {
            issue.kind == VersionScanIssueKind::LoaderIdentityMalformed
                && issue.version_id.as_deref() == Some(version_id)
        }));

        fs::remove_dir_all(mc_dir).expect("remove temp test dir");
    }

    fn unique_test_dir(name: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time ok")
            .as_nanos();
        std::env::temp_dir().join(format!("axial-{name}-{unique}"))
    }
}
