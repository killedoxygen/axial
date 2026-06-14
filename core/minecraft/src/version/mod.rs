use crate::loaders::types::{LoaderBuildMetadata, LoaderComponentId};
use crate::paths::versions_dir;
use crate::types::{VersionEntry, VersionLoaderAttachment, VersionSubjectKind};
use crate::version_meta::{analyze_minecraft_version, compare_version_entries};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::Path;

const LOADER_METADATA_FILE: &str = ".croopor-loader.json";

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
    java_version: Option<JavaVersionStub>,
}

#[derive(Debug, Clone, Deserialize)]
struct JavaVersionStub {
    #[serde(default)]
    component: String,
    #[serde(rename = "majorVersion", default)]
    major_version: i32,
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

pub fn scan_versions(mc_dir: &Path) -> std::io::Result<Vec<VersionEntry>> {
    let versions_dir = versions_dir(mc_dir);
    let entries = fs::read_dir(&versions_dir)?;
    let mut stubs = HashMap::new();
    let mut loader_metadata = HashMap::new();

    for entry in entries.filter_map(Result::ok) {
        if !entry.path().is_dir() {
            continue;
        }
        let id = entry.file_name().to_string_lossy().to_string();
        let json_path = entry.path().join(format!("{id}.json"));
        let Ok(data) = fs::read_to_string(&json_path) else {
            continue;
        };
        let Ok(stub) = serde_json::from_str::<VersionStub>(&data) else {
            continue;
        };
        if let Some(metadata) = read_installed_loader_metadata(&entry.path()) {
            loader_metadata.insert(id.clone(), metadata);
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
        let (launchable, status, status_detail, needs_install) = if incomplete_marker.exists() {
            (
                false,
                "incomplete".to_string(),
                "Installation incomplete".to_string(),
                id.clone(),
            )
        } else if effective_parent.is_empty() {
            if jar_path.is_file() {
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
            if !parent_json.is_file() {
                (
                    false,
                    "incomplete".to_string(),
                    format!("Base version {} needs to be installed", effective_parent),
                    effective_parent.clone(),
                )
            } else if !parent_jar.is_file() {
                (
                    false,
                    "incomplete".to_string(),
                    format!("Base version {} needs to be downloaded", effective_parent),
                    effective_parent.clone(),
                )
            } else if jar_path.is_file() {
                (true, "ready".to_string(), String::new(), String::new())
            } else {
                (
                    false,
                    "incomplete".to_string(),
                    "Loader files are not fully installed".to_string(),
                    id.clone(),
                )
            }
        };

        let loader = metadata.map(loader_attachment_from_metadata);
        let analysis = analyze_minecraft_version(id, &stub.kind, &stub.release_time, None, &[]);

        versions.push(VersionEntry {
            subject_kind: VersionSubjectKind::InstalledVersion,
            id: id.clone(),
            raw_kind: stub.kind.clone(),
            release_time: stub.release_time.clone(),
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
    Ok(versions)
}

fn resolve_java_version(
    id: &str,
    stubs: &HashMap<String, VersionStub>,
    loader_metadata: &HashMap<String, InstalledLoaderMetadata>,
) -> JavaVersionStub {
    let mut current_id = id.to_string();
    let mut current = stubs.get(&current_id);
    let mut fallback_parent = String::new();
    while let Some(stub) = current {
        if let Some(java_version) = &stub.java_version {
            return java_version.clone();
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
        return java_version.clone();
    }

    JavaVersionStub {
        component: String::new(),
        major_version: 0,
    }
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

fn read_installed_loader_metadata(version_dir: &Path) -> Option<InstalledLoaderMetadata> {
    let data = fs::read(version_dir.join(LOADER_METADATA_FILE)).ok()?;
    let metadata = serde_json::from_slice::<InstalledLoaderMetadata>(&data).ok()?;
    if metadata.schema_version != 1
        || metadata.build_id.trim().is_empty()
        || metadata.minecraft_version.trim().is_empty()
        || metadata.loader_version.trim().is_empty()
    {
        return None;
    }
    Some(metadata)
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

#[cfg(test)]
mod tests {
    use super::{
        InstalledLoaderMetadata, JavaVersionStub, VersionStub, resolve_java_version, scan_versions,
    };
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
            },
        );
        stubs.insert(
            "1.20.1".to_string(),
            VersionStub {
                kind: "release".to_string(),
                release_time: String::new(),
                inherits_from: String::new(),
                java_version: Some(JavaVersionStub {
                    component: "java-runtime-gamma".to_string(),
                    major_version: 17,
                }),
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
            forge_dir.join(".croopor-loader.json"),
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
        std::env::temp_dir().join(format!("croopor-{name}-{unique}"))
    }
}
