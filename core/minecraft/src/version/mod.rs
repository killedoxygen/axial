use crate::launch::{Downloads, JavaVersion, effective_java_version_for};
use crate::loaders::{MaterializedLoaderProfile, validate_materialized_loader_profile};
use crate::managed_fs::{
    ManagedDir, ManagedDirectoryIdentity, ManagedLibraryOperation, ManagedLibraryWitness,
    ManagedPassiveFileRevision,
};
use crate::managed_publication::{ManagedPublicationError, ManagedRootPublicationReadLease};
use crate::portable_path::{PortableFileName, PortablePathKey};
use crate::types::{VersionEntry, VersionLoaderAttachment, VersionSubjectKind};
use crate::version_meta::{analyze_minecraft_version, compare_version_entries};
use axial_fs::{DirectoryRevision, EntryKind};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::io;

const MAX_VERSION_SCAN_ENTRIES: usize = 4096;
const MAX_VERSION_DIRECTORY_SCAN_ENTRIES: usize = 64;
const MAX_VERSION_SCAN_WORK: usize = 16_384;

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
    operation: ManagedLibraryOperation,
    lease: ManagedRootPublicationReadLease,
}

impl VersionBundleReadGuard {
    pub fn acquire(operation: &ManagedLibraryOperation) -> io::Result<Self> {
        operation.revalidate()?;
        let operation = operation.clone();
        let root = operation
            .managed_directory()
            .map_err(io::Error::other)?;
        let lease =
            ManagedRootPublicationReadLease::acquire(root).map_err(publication_read_error)?;
        Ok(Self {
            operation,
            lease,
        })
    }

    pub fn revalidate(&self) -> io::Result<()> {
        self.operation.revalidate()?;
        self.lease.revalidate().map_err(publication_read_error)
    }

    fn root_binding(&self) -> io::Result<ManagedDirectoryIdentity> {
        self.lease
            .root_identity()
            .map_err(publication_read_error)
    }
}

impl VersionScanSnapshot {
    pub fn dependencies(&self) -> &VersionScanDependencyStamp {
        &self.dependencies
    }
}

#[derive(Clone)]
pub struct VersionScanDependencyStamp {
    library: ManagedLibraryWitness,
    root_binding: ManagedDirectoryIdentity,
    facts: VersionScanDependencyFacts,
}

impl VersionScanDependencyStamp {
    pub fn is_revalidated(&self) -> bool {
        let Ok(operation) = self.library.try_acquire() else {
            return false;
        };
        let Ok(root) = operation.managed_directory() else {
            return false;
        };
        let Ok(publication_read) = ManagedRootPublicationReadLease::acquire(root.clone()) else {
            return false;
        };
        let valid = publication_read
            .root_identity()
            .is_ok_and(|binding| binding == self.root_binding)
            && self.facts.is_revalidated(&root)
            && publication_read.revalidate().is_ok()
            && operation.revalidate().is_ok();
        drop(publication_read);
        valid
    }
}

#[derive(Clone)]
enum VersionScanDependencyFacts {
    Invalid,
    MissingVersions,
    Present {
        revision: DirectoryRevision,
        versions: Vec<VersionDirectoryFact>,
    },
}

#[derive(Clone)]
struct VersionDirectoryFact {
    id: String,
    revision: DirectoryRevision,
    files: Vec<VersionFileFact>,
}

#[derive(Clone)]
struct VersionFileFact {
    name: String,
    revision: ManagedPassiveFileRevision,
}

impl VersionScanDependencyFacts {
    fn is_revalidated(&self, root: &ManagedDir) -> bool {
        match self {
            Self::Invalid => false,
            Self::MissingVersions => {
                matches!(root.has_portably_exact_child_name("versions"), Ok(false))
            }
            Self::Present {
                revision,
                versions: expected,
            } => {
                if !matches!(root.has_portably_exact_child_name("versions"), Ok(true)) {
                    return false;
                }
                let Ok(versions) = root.open_child("versions") else {
                    return false;
                };
                if versions.validate_passive_revision(revision).is_err() {
                    return false;
                }
                expected.iter().all(|expected| {
                    let Ok(version) = versions.open_child(&expected.id) else {
                        return false;
                    };
                    version
                        .validate_passive_revision(&expected.revision)
                        .is_ok()
                        && expected.files.iter().all(|file| {
                            version
                                .validate_passive_file_revision(&file.name, &file.revision)
                                .is_ok()
                        })
                })
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
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

pub fn scan_versions(operation: &ManagedLibraryOperation) -> io::Result<Vec<VersionEntry>> {
    scan_versions_report(operation).map(|report| report.versions)
}

pub fn scan_versions_report(operation: &ManagedLibraryOperation) -> io::Result<VersionScanReport> {
    scan_versions_snapshot(operation).map(|snapshot| snapshot.report)
}

pub fn scan_versions_snapshot(operation: &ManagedLibraryOperation) -> io::Result<VersionScanSnapshot> {
    let publication_read = VersionBundleReadGuard::acquire(operation)?;
    let root = publication_read
        .operation
        .managed_directory()
        .map_err(io::Error::other)?;
    match root.has_portably_exact_child_name("versions") {
        Ok(false) => {
            return finish_scan_snapshot(
                VersionScanReport {
                    state: VersionScanState::Empty,
                    versions: Vec::new(),
                    issues: Vec::new(),
                },
                VersionScanDependencyFacts::MissingVersions,
                operation,
                &publication_read,
            );
        }
        Ok(true) => {}
        Err(_) => {
            return finish_scan_snapshot(
                VersionScanReport {
                    state: VersionScanState::Degraded,
                    versions: Vec::new(),
                    issues: vec![version_scan_issue(
                        VersionScanIssueKind::VersionsDirectoryUnreadable,
                        None,
                    )],
                },
                VersionScanDependencyFacts::Invalid,
                operation,
                &publication_read,
            );
        }
    }
    let versions_root = match root.open_child("versions") {
        Ok(versions) => versions,
        Err(error) if is_not_found_loader_error(&error) => {
            return finish_scan_snapshot(
                VersionScanReport {
                    state: VersionScanState::Empty,
                    versions: Vec::new(),
                    issues: Vec::new(),
                },
                VersionScanDependencyFacts::MissingVersions,
                operation,
                &publication_read,
            );
        }
        Err(_) => {
            return finish_scan_snapshot(
                VersionScanReport {
                    state: VersionScanState::Degraded,
                    versions: Vec::new(),
                    issues: vec![version_scan_issue(
                        VersionScanIssueKind::VersionsDirectoryUnreadable,
                        None,
                    )],
                },
                VersionScanDependencyFacts::Invalid,
                operation,
                &publication_read,
            );
        }
    };
    let versions_revision = versions_root
        .passive_revision()
        .map_err(io::Error::other)?;
    let entries = versions_root
        .guarded_entries_bounded(MAX_VERSION_SCAN_ENTRIES)
        .map_err(io::Error::other)?;
    let mut remaining_scan_work = MAX_VERSION_SCAN_WORK
        .checked_sub(entries.len())
        .ok_or_else(|| io::Error::other("version scan work budget underflowed"))?;
    let mut stubs = HashMap::new();
    let mut loader_profiles = HashMap::new();
    let mut issues = Vec::new();
    let mut names = HashSet::<PortablePathKey>::new();
    let mut guarded_versions = HashMap::<String, VersionDirectoryScan>::new();
    let mut json_present = HashSet::new();
    let mut dependencies_revalidatable = true;

    for entry in entries {
        let Some(id) = entry.utf8_name() else {
            issues.push(version_scan_issue(
                VersionScanIssueKind::VersionDirectoryEntryUnreadable,
                None,
            ));
            continue;
        };
        let Ok(portable_id) = PortableFileName::new_exact(id) else {
            issues.push(version_scan_issue(
                VersionScanIssueKind::VersionDirectoryEntryUnreadable,
                None,
            ));
            continue;
        };
        if !names.insert(portable_id.key()) {
            issues.push(version_scan_issue(
                VersionScanIssueKind::VersionDirectoryEntryUnreadable,
                Some(id.to_string()),
            ));
            continue;
        }
        match entry.kind() {
            EntryKind::File => continue,
            EntryKind::Link | EntryKind::Other => {
                issues.push(version_scan_issue(
                    VersionScanIssueKind::VersionDirectoryEntryUnreadable,
                    Some(id.to_string()),
                ));
                continue;
            }
            EntryKind::Directory => {}
        }
        let version_dir = match versions_root.open_observed_child(&entry) {
            Ok(directory) => directory,
            Err(_) => {
                dependencies_revalidatable = false;
                issues.push(version_scan_issue(
                    VersionScanIssueKind::VersionDirectoryEntryUnreadable,
                    Some(id.to_string()),
                ));
                continue;
            }
        };
        let revision = version_dir.passive_revision().map_err(io::Error::other)?;
        let mut guarded = VersionDirectoryScan {
            directory: version_dir,
            revision,
            entries: HashMap::new(),
            files: HashMap::new(),
        };
        let id = id.to_string();
        let entry_validation = guarded.validate_exact_entries(&mut remaining_scan_work);
        if entry_validation != VersionDirectoryEntryValidation::Valid {
            if entry_validation == VersionDirectoryEntryValidation::Unrevalidatable {
                dependencies_revalidatable = false;
            }
            issues.push(version_scan_issue(
                VersionScanIssueKind::VersionDirectoryEntryUnreadable,
                Some(id.clone()),
            ));
            guarded_versions.insert(id, guarded);
            continue;
        }
        let reserved_loader_id = crate::loaders::api::is_reserved_installed_loader_id(&id);
        let json_name = format!("{id}.json");
        let json_guard = match guarded.observe_file(&json_name) {
            Ok(Some(guard)) => {
                json_present.insert(id.clone());
                guard
            }
            Ok(None) => {
                issues.push(version_scan_issue(
                    VersionScanIssueKind::VersionJsonMissing,
                    Some(id.clone()),
                ));
                guarded_versions.insert(id, guarded);
                continue;
            }
            Err(_) => {
                dependencies_revalidatable = false;
                issues.push(version_scan_issue(
                    VersionScanIssueKind::VersionJsonUnreadable,
                    Some(id.clone()),
                ));
                guarded_versions.insert(id, guarded);
                continue;
            }
        };
        let data = match guarded.directory.read_guarded_file_bounded(
            &json_name,
            &json_guard,
            crate::known_good::MAX_KNOWN_GOOD_VERSION_JSON_BYTES as u64,
        ) {
            Ok(data) => data,
            Err(_) => {
                dependencies_revalidatable = false;
                issues.push(version_scan_issue(
                    VersionScanIssueKind::VersionJsonUnreadable,
                    Some(id.clone()),
                ));
                guarded_versions.insert(id, guarded);
                continue;
            }
        };
        let stub = match serde_json::from_slice::<VersionStub>(&data) {
            Ok(stub) => stub,
            Err(_) => {
                issues.push(version_scan_issue(
                    VersionScanIssueKind::VersionJsonMalformed,
                    Some(id.clone()),
                ));
                guarded_versions.insert(id, guarded);
                continue;
            }
        };
        if !stub.inherits_from.is_empty()
            && PortableFileName::new_exact(&stub.inherits_from).is_err()
        {
            issues.push(version_scan_issue(
                VersionScanIssueKind::LoaderIdentityMalformed,
                Some(id.clone()),
            ));
            guarded_versions.insert(id, guarded);
            continue;
        }
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
                    Some(id.clone()),
                ));
                guarded_versions.insert(id, guarded);
                continue;
            }
            Err(_) => {}
        }
        stubs.insert(id.clone(), stub);
        guarded_versions.insert(id, guarded);
    }

    let mut jars_present = HashSet::new();
    for (id, guarded) in &mut guarded_versions {
        let jar_name = format!("{id}.jar");
        match guarded.observe_file(&jar_name) {
            Ok(Some(_)) => {
                jars_present.insert(id.clone());
            }
            Ok(None) => {}
            Err(_) => {
                dependencies_revalidatable = false;
                issues.push(version_scan_issue(
                    VersionScanIssueKind::VersionDirectoryEntryUnreadable,
                    Some(id.clone()),
                ));
            }
        }
    }

    let mut versions = Vec::new();
    for (id, stub) in &stubs {
        let loader_profile = loader_profiles.get(id);
        let effective_parent = stub.inherits_from.clone();
        let resolved_java = resolve_java_version(id, &stubs);
        let (launchable, status, status_detail, needs_install) = if effective_parent.is_empty() {
            let jar_ready = jars_present.contains(id);
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
            let child_has_client_artifact = stub.downloads.client.is_some();
            let parent_json_ready = json_present.contains(&effective_parent);
            if !parent_json_ready {
                (
                    false,
                    "incomplete".to_string(),
                    format!("Base version {} needs to be installed", effective_parent),
                    effective_parent.clone(),
                )
            } else {
                let jar_ready = jars_present.contains(id);
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
                    let parent_jar_ready = jars_present.contains(&effective_parent);
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
    issues.sort_by(|left, right| {
        left.version_id
            .cmp(&right.version_id)
            .then_with(|| left.kind.cmp(&right.kind))
    });
    let state = if !issues.is_empty() {
        VersionScanState::Degraded
    } else if versions.is_empty() {
        VersionScanState::Empty
    } else {
        VersionScanState::Ready
    };
    versions_root
        .validate_passive_revision(&versions_revision)
        .map_err(io::Error::other)?;
    let mut version_facts = Vec::with_capacity(guarded_versions.len());
    for (id, guarded) in guarded_versions {
        guarded
            .directory
            .validate_passive_revision(&guarded.revision)
            .map_err(io::Error::other)?;
        version_facts.push(VersionDirectoryFact {
            id,
            revision: guarded.revision,
            files: guarded
                .files
                .into_iter()
                .map(|(name, revision)| VersionFileFact { name, revision })
                .collect(),
        });
    }
    version_facts.sort_by(|left, right| left.id.cmp(&right.id));
    let facts = if dependencies_revalidatable {
        VersionScanDependencyFacts::Present {
            revision: versions_revision,
            versions: version_facts,
        }
    } else {
        VersionScanDependencyFacts::Invalid
    };
    finish_scan_snapshot(
        VersionScanReport {
            state,
            versions,
            issues,
        },
        facts,
        operation,
        &publication_read,
    )
}

fn finish_scan_snapshot(
    report: VersionScanReport,
    facts: VersionScanDependencyFacts,
    operation: &ManagedLibraryOperation,
    publication_read: &VersionBundleReadGuard,
) -> io::Result<VersionScanSnapshot> {
    publication_read.revalidate()?;
    if !matches!(facts, VersionScanDependencyFacts::Invalid) {
        let root = publication_read
            .operation
            .managed_directory()
            .map_err(io::Error::other)?;
        if !facts.is_revalidated(&root) {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "version scan dependencies changed before completion",
            ));
        }
    }
    let root_binding = publication_read.root_binding()?;
    Ok(VersionScanSnapshot {
        report,
        dependencies: VersionScanDependencyStamp {
            library: operation.witness(),
            root_binding,
            facts,
        },
    })
}

struct VersionDirectoryScan {
    directory: ManagedDir,
    revision: DirectoryRevision,
    entries: HashMap<String, EntryKind>,
    files: HashMap<String, ManagedPassiveFileRevision>,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum VersionDirectoryEntryValidation {
    Valid,
    ObservedInvalid,
    Unrevalidatable,
}

impl VersionDirectoryScan {
    fn validate_exact_entries(
        &mut self,
        remaining_scan_work: &mut usize,
    ) -> VersionDirectoryEntryValidation {
        let limit = (*remaining_scan_work).min(MAX_VERSION_DIRECTORY_SCAN_ENTRIES);
        if limit == 0 {
            return VersionDirectoryEntryValidation::Unrevalidatable;
        }
        *remaining_scan_work -= limit;
        let mut names = HashSet::<PortablePathKey>::new();
        let entries = match self.directory.guarded_entries_bounded(limit) {
            Ok(entries) => entries,
            Err(_) => return VersionDirectoryEntryValidation::Unrevalidatable,
        };
        *remaining_scan_work += limit - entries.len();
        for entry in entries {
            let Some(name) = entry.utf8_name() else {
                return VersionDirectoryEntryValidation::ObservedInvalid;
            };
            let Ok(name) = PortableFileName::new_exact(name) else {
                return VersionDirectoryEntryValidation::ObservedInvalid;
            };
            if !names.insert(name.key()) {
                return VersionDirectoryEntryValidation::ObservedInvalid;
            }
            if matches!(entry.kind(), EntryKind::Link | EntryKind::Other) {
                return VersionDirectoryEntryValidation::ObservedInvalid;
            }
            self.entries.insert(name.as_str().to_string(), entry.kind());
        }
        VersionDirectoryEntryValidation::Valid
    }

    fn observe_file(
        &mut self,
        name: &str,
    ) -> Result<Option<crate::managed_fs::ManagedFileGuard>, crate::loaders::LoaderError> {
        match self.entries.get(name) {
            None => return Ok(None),
            Some(EntryKind::File) => {}
            Some(EntryKind::Directory | EntryKind::Link | EntryKind::Other) => {
                return Err(crate::loaders::LoaderError::Verify(
                    "version file name does not identify an exact regular file".to_string(),
                ));
            }
        }
        let guard = self.directory.inspect_regular_file(name)?;
        if let Some(guard) = guard.as_ref() {
            self.files
                .insert(name.to_string(), guard.passive_revision());
        }
        Ok(guard)
    }
}

fn is_not_found_loader_error(error: &crate::loaders::LoaderError) -> bool {
    matches!(error, crate::loaders::LoaderError::Io(error) if error.kind() == io::ErrorKind::NotFound)
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
    let mut visited = HashSet::new();
    while let Some(stub) = current {
        if !visited.insert(current_id.clone()) {
            break;
        }
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
        VersionScanIssueKind, VersionScanState, VersionStub, resolve_java_version,
    };
    use crate::launch::{Downloads, JavaVersion};
    use crate::loaders::installed_version_id_for;
    use crate::loaders::types::{LoaderComponentId, LoaderSelectionReason, LoaderTerm};
    use std::collections::HashMap;
    use std::fs;
    use std::path::{Path, PathBuf};
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
    fn dependency_stamp_rejects_equivalent_replacement_library_root() {
        let mc_dir = unique_test_dir("replacement-library-root");
        let displaced = mc_dir.with_extension("displaced");
        fs::create_dir_all(&mc_dir).expect("create original library root");
        let snapshot = scan_versions_snapshot(&mc_dir).expect("scan original root");

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

    fn scan_versions(path: &Path) -> std::io::Result<Vec<crate::types::VersionEntry>> {
        let root = crate::managed_fs::ManagedLibraryRoot::open_for_test(path)?;
        let operation = root.try_acquire()?;
        super::scan_versions(&operation)
    }

    fn scan_versions_report(path: &Path) -> std::io::Result<super::VersionScanReport> {
        let root = crate::managed_fs::ManagedLibraryRoot::open_for_test(path)?;
        let operation = root.try_acquire()?;
        super::scan_versions_report(&operation)
    }

    fn scan_versions_snapshot(path: &Path) -> std::io::Result<super::VersionScanSnapshot> {
        let root = crate::managed_fs::ManagedLibraryRoot::open_for_test(path)?;
        let operation = root.try_acquire()?;
        super::scan_versions_snapshot(&operation)
    }
}
