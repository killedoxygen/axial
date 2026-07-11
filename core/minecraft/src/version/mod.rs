use crate::launch::{Downloads, JavaVersion, effective_java_version_for};
use crate::loaders::types::{LoaderBuildMetadata, LoaderComponentId};
use crate::paths::versions_dir;
use crate::types::{VersionEntry, VersionLoaderAttachment, VersionSubjectKind};
use crate::version_meta::{analyze_minecraft_version, compare_version_entries};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::Path;
use std::time::SystemTime;

const LOADER_METADATA_FILE: &str = ".axial-loader.json";

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

impl VersionScanSnapshot {
    pub fn dependencies(&self) -> &VersionScanDependencyStamp {
        &self.dependencies
    }
}

#[derive(Clone)]
pub struct VersionScanDependencyStamp {
    observations: Vec<DependencyObservation>,
    revalidatable: bool,
}

impl VersionScanDependencyStamp {
    pub fn is_revalidated(&self) -> bool {
        self.revalidatable
            && self
                .observations
                .iter()
                .all(DependencyObservation::is_revalidated)
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

    fn read(&mut self, path: &Path) -> io::Result<Vec<u8>> {
        let before = self.begin(path);
        let result = fs::read(path);
        self.finish(path, before);
        result
    }

    fn is_dir(&mut self, path: &Path) -> bool {
        self.observe_kind(path) == Some(DependencyKind::Directory)
    }

    fn is_file(&mut self, path: &Path) -> bool {
        self.observe_kind(path) == Some(DependencyKind::File)
    }

    fn exists(&mut self, path: &Path) -> bool {
        self.observe_kind(path).is_some()
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

    fn into_stamp(self) -> VersionScanDependencyStamp {
        VersionScanDependencyStamp {
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
    LoaderMetadataUnreadable,
    LoaderMetadataMalformed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedVersion {
    pub id: String,
    #[serde(default)]
    pub inherits_from: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct VersionStub {
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(rename = "releaseTime", default)]
    release_time: String,
    #[serde(rename = "inheritsFrom", default)]
    inherits_from: String,
    #[serde(rename = "javaVersion", default)]
    java_version: Option<JavaVersion>,
    #[serde(default)]
    downloads: Downloads,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct InstalledLoaderMetadata {
    #[serde(default)]
    schema_version: u32,
    component_id: LoaderComponentId,
    #[serde(default)]
    component_name: String,
    build_id: String,
    minecraft_version: String,
    loader_version: String,
    #[serde(default)]
    build_meta: LoaderBuildMetadata,
}

pub fn scan_versions(mc_dir: &Path) -> io::Result<Vec<VersionEntry>> {
    scan_versions_report(mc_dir).map(|report| report.versions)
}

pub fn scan_versions_report(mc_dir: &Path) -> io::Result<VersionScanReport> {
    scan_versions_snapshot(mc_dir).map(|snapshot| snapshot.report)
}

pub fn scan_versions_snapshot(mc_dir: &Path) -> io::Result<VersionScanSnapshot> {
    let versions_dir = versions_dir(mc_dir);
    let mut dependencies = DependencyTracker::new();
    let versions_root_before = dependencies.begin(&versions_dir);
    let entries = match fs::read_dir(&versions_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            dependencies.finish(&versions_dir, versions_root_before);
            return Ok(finish_scan_snapshot(
                VersionScanReport {
                    state: VersionScanState::Empty,
                    versions: Vec::new(),
                    issues: Vec::new(),
                },
                dependencies,
            ));
        }
        Err(_) => {
            dependencies.finish(&versions_dir, versions_root_before);
            return Ok(finish_scan_snapshot(
                VersionScanReport {
                    state: VersionScanState::Degraded,
                    versions: Vec::new(),
                    issues: vec![version_scan_issue(
                        VersionScanIssueKind::VersionsDirectoryUnreadable,
                        None,
                    )],
                },
                dependencies,
            ));
        }
    };
    let mut stubs = HashMap::new();
    let mut loader_metadata = HashMap::new();
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
        let json_path = entry_path.join(format!("{id}.json"));
        let data_result = dependencies.read_to_string(&json_path);
        let data = match data_result {
            Ok(data) => data,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let incomplete_marker = entry_path.join(".incomplete");
                let incomplete = dependencies.exists(&incomplete_marker);
                if incomplete {
                    continue;
                }
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
        match read_installed_loader_metadata(&entry_path, &mut dependencies) {
            LoaderMetadataScan::Ready(metadata) => {
                loader_metadata.insert(id.clone(), metadata);
            }
            LoaderMetadataScan::Missing => {}
            LoaderMetadataScan::Unreadable => issues.push(version_scan_issue(
                VersionScanIssueKind::LoaderMetadataUnreadable,
                Some(id.clone()),
            )),
            LoaderMetadataScan::Malformed => issues.push(version_scan_issue(
                VersionScanIssueKind::LoaderMetadataMalformed,
                Some(id.clone()),
            )),
        }
        stubs.insert(id, stub);
    }

    let mut versions = Vec::new();
    for (id, stub) in &stubs {
        let metadata = loader_metadata.get(id);
        let effective_parent = effective_parent_version(&stub.inherits_from, metadata);
        let jar_path = versions_dir.join(id).join(format!("{id}.jar"));
        let incomplete_marker = versions_dir.join(id).join(".incomplete");

        let resolved_java = resolve_java_version(id, &stubs, &loader_metadata);
        let incomplete = dependencies.exists(&incomplete_marker);
        let (launchable, status, status_detail, needs_install) = if incomplete {
            (
                false,
                "incomplete".to_string(),
                "Installation incomplete".to_string(),
                id.clone(),
            )
        } else if effective_parent.is_empty() {
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
        let loader = metadata.map(loader_attachment_from_metadata);

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
            manifest_url: String::new(),
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
    Ok(finish_scan_snapshot(
        VersionScanReport {
            state,
            versions,
            issues,
        },
        dependencies,
    ))
}

fn finish_scan_snapshot(
    report: VersionScanReport,
    dependencies: DependencyTracker,
) -> VersionScanSnapshot {
    VersionScanSnapshot {
        report,
        dependencies: dependencies.into_stamp(),
    }
}

fn resolve_java_version(
    id: &str,
    stubs: &HashMap<String, VersionStub>,
    loader_metadata: &HashMap<String, InstalledLoaderMetadata>,
) -> JavaVersion {
    let mut current_id = id.to_string();
    let mut current = stubs.get(&current_id);
    let mut fallback_parent = String::new();
    while let Some(stub) = current {
        if let Some(java_version) = &stub.java_version {
            return effective_java_version_for(&current_id, &stub.kind, java_version);
        }
        let next_parent =
            effective_parent_version(&stub.inherits_from, loader_metadata.get(&current_id));
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

fn effective_parent_version(
    declared_parent: &str,
    metadata: Option<&InstalledLoaderMetadata>,
) -> String {
    if !declared_parent.trim().is_empty() {
        return declared_parent.to_string();
    }
    metadata
        .map(|metadata| metadata.minecraft_version.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_default()
}

enum LoaderMetadataScan {
    Ready(InstalledLoaderMetadata),
    Missing,
    Unreadable,
    Malformed,
}

fn read_installed_loader_metadata(
    version_dir: &Path,
    dependencies: &mut DependencyTracker,
) -> LoaderMetadataScan {
    let path = version_dir.join(LOADER_METADATA_FILE);
    let result = dependencies.read(&path);
    let data = match result {
        Ok(data) => data,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return LoaderMetadataScan::Missing;
        }
        Err(_) => return LoaderMetadataScan::Unreadable,
    };
    let Ok(metadata) = serde_json::from_slice::<InstalledLoaderMetadata>(&data) else {
        return LoaderMetadataScan::Malformed;
    };
    if metadata.schema_version != 1
        || metadata.build_id.trim().is_empty()
        || metadata.minecraft_version.trim().is_empty()
        || metadata.loader_version.trim().is_empty()
    {
        return LoaderMetadataScan::Malformed;
    }
    LoaderMetadataScan::Ready(metadata)
}

fn loader_attachment_from_metadata(metadata: &InstalledLoaderMetadata) -> VersionLoaderAttachment {
    VersionLoaderAttachment {
        component_id: metadata.component_id,
        component_name: if metadata.component_name.trim().is_empty() {
            metadata.component_id.display_name().to_string()
        } else {
            metadata.component_name.clone()
        },
        build_id: metadata.build_id.clone(),
        loader_version: metadata.loader_version.clone(),
        build_meta: metadata.build_meta.clone(),
    }
}

fn version_scan_issue(kind: VersionScanIssueKind, version_id: Option<String>) -> VersionScanIssue {
    VersionScanIssue { kind, version_id }
}

#[cfg(test)]
mod tests {
    use super::{
        InstalledLoaderMetadata, VersionScanIssueKind, VersionScanState, VersionStub,
        resolve_java_version, scan_versions, scan_versions_report,
    };
    use crate::launch::{Downloads, JavaVersion};
    use crate::loaders::types::{
        LoaderBuildMetadata, LoaderComponentId, LoaderSelectionMeta, LoaderSelectionReason,
        LoaderSelectionSource, LoaderTerm, LoaderTermEvidence, LoaderTermSource,
    };
    use std::collections::HashMap;
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn resolve_java_version_follows_metadata_parent_chain_for_loader_versions() {
        let mut stubs = HashMap::new();
        stubs.insert(
            "fabric-loader-0.14.21-1.20.1".to_string(),
            VersionStub {
                kind: "release".to_string(),
                release_time: String::new(),
                inherits_from: String::new(),
                java_version: None,
                downloads: Downloads::default(),
            },
        );
        stubs.insert(
            "1.20.1".to_string(),
            VersionStub {
                kind: "release".to_string(),
                release_time: String::new(),
                inherits_from: String::new(),
                java_version: Some(JavaVersion {
                    component: "java-runtime-gamma".to_string(),
                    major_version: 17,
                }),
                downloads: Downloads::default(),
            },
        );
        let mut metadata = HashMap::new();
        metadata.insert(
            "fabric-loader-0.14.21-1.20.1".to_string(),
            test_loader_metadata(LoaderComponentId::Fabric, "1.20.1", "0.14.21"),
        );

        let resolved = resolve_java_version("fabric-loader-0.14.21-1.20.1", &stubs, &metadata);

        assert_eq!(resolved.component, "java-runtime-gamma");
        assert_eq!(resolved.major_version, 17);
    }

    #[test]
    fn scan_versions_marks_missing_parent_as_install_target() {
        let mc_dir = unique_test_dir("missing-parent-install-target");
        let versions_dir = mc_dir.join("versions");
        let child_dir = versions_dir.join("fabric-loader-0.14.21-1.20.1");
        fs::create_dir_all(&child_dir).expect("create child version dir");
        fs::write(
            child_dir.join("fabric-loader-0.14.21-1.20.1.json"),
            r#"{
                "id":"fabric-loader-0.14.21-1.20.1",
                "inheritsFrom":"1.20.1",
                "type":"release"
            }"#,
        )
        .expect("write child json");

        let versions = scan_versions(&mc_dir).expect("scan versions");
        let version = versions
            .iter()
            .find(|entry| entry.id == "fabric-loader-0.14.21-1.20.1")
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
    fn scan_versions_reads_loader_lifecycle_from_installed_metadata() {
        let mc_dir = unique_test_dir("loader-lifecycle-metadata");
        let versions_dir = mc_dir.join("versions");
        let forge_dir = versions_dir.join("26.1.2-forge-64.0.4");
        fs::create_dir_all(&forge_dir).expect("create forge version dir");
        fs::write(
            forge_dir.join("26.1.2-forge-64.0.4.json"),
            r#"{
                "id":"26.1.2-forge-64.0.4",
                "inheritsFrom":"26.1.2",
                "type":"release"
            }"#,
        )
        .expect("write forge json");

        let metadata = InstalledLoaderMetadata {
            build_meta: LoaderBuildMetadata {
                terms: vec![LoaderTerm::Beta, LoaderTerm::Latest],
                evidence: vec![
                    LoaderTermEvidence {
                        term: LoaderTerm::Beta,
                        source: LoaderTermSource::ExplicitVersionLabel,
                    },
                    LoaderTermEvidence {
                        term: LoaderTerm::Latest,
                        source: LoaderTermSource::PromotionMarker,
                    },
                ],
                selection: LoaderSelectionMeta {
                    default_rank: 650,
                    reason: LoaderSelectionReason::LatestUnstable,
                    source: LoaderSelectionSource::AbsenceOfRecommended,
                },
                display_tags: vec!["latest".to_string(), "beta".to_string()],
            },
            ..test_loader_metadata(LoaderComponentId::Forge, "26.1.2", "64.0.4")
        };
        fs::write(
            forge_dir.join(".axial-loader.json"),
            serde_json::to_vec_pretty(&metadata).expect("serialize metadata"),
        )
        .expect("write metadata");

        let versions = scan_versions(&mc_dir).expect("scan versions");
        let version = versions
            .iter()
            .find(|entry| entry.id == "26.1.2-forge-64.0.4")
            .expect("forge version exists");

        let loader = version.loader.as_ref().expect("loader lifecycle exists");
        assert_eq!(loader.component_id, LoaderComponentId::Forge);
        assert_eq!(loader.loader_version, "64.0.4");
        assert!(loader.build_meta.terms.contains(&LoaderTerm::Beta));
        assert_eq!(
            loader.build_meta.selection.reason,
            LoaderSelectionReason::LatestUnstable
        );

        fs::remove_dir_all(&mc_dir).expect("remove temp test dir");
    }

    #[test]
    fn scan_versions_anchors_loader_metadata_to_base_minecraft_version() {
        let mc_dir = unique_test_dir("loader-base-version-metadata");
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

        let fabric_dir = versions_dir.join("fabric-loader-0.19.3-1.21.5");
        fs::create_dir_all(&fabric_dir).expect("create fabric version dir");
        fs::write(
            fabric_dir.join("fabric-loader-0.19.3-1.21.5.json"),
            r#"{
                "id":"fabric-loader-0.19.3-1.21.5",
                "mainClass":"net.fabricmc.loader.impl.launch.knot.KnotClient",
                "libraries":[]
            }"#,
        )
        .expect("write fabric json");
        fs::write(
            fabric_dir.join(".axial-loader.json"),
            serde_json::to_vec_pretty(&test_loader_metadata(
                LoaderComponentId::Fabric,
                "1.21.5",
                "0.19.3",
            ))
            .expect("serialize metadata"),
        )
        .expect("write metadata");

        let versions = scan_versions(&mc_dir).expect("scan versions");
        let version = versions
            .iter()
            .find(|entry| entry.id == "fabric-loader-0.19.3-1.21.5")
            .expect("fabric version exists");

        assert_eq!(version.inherits_from, "1.21.5");
        assert_eq!(version.raw_kind, "release");
        assert_eq!(version.release_time, "2025-03-25T12:00:00+00:00");
        assert_eq!(version.minecraft_meta.display_name, "1.21.5");
        assert_eq!(version.minecraft_meta.effective_version, "1.21.5");

        fs::remove_dir_all(&mc_dir).expect("remove temp test dir");
    }

    fn test_loader_metadata(
        component_id: LoaderComponentId,
        minecraft_version: &str,
        loader_version: &str,
    ) -> InstalledLoaderMetadata {
        InstalledLoaderMetadata {
            schema_version: 1,
            component_id,
            component_name: component_id.display_name().to_string(),
            build_id: format!(
                "{}:{}:{}",
                component_id.short_key(),
                minecraft_version,
                loader_version
            ),
            minecraft_version: minecraft_version.to_string(),
            loader_version: loader_version.to_string(),
            build_meta: LoaderBuildMetadata::default(),
        }
    }

    fn unique_test_dir(name: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time ok")
            .as_nanos();
        std::env::temp_dir().join(format!("axial-{name}-{unique}"))
    }
}
