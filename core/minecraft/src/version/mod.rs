use crate::loaders::{
    infer_build_from_version_id,
    types::{CachedCatalog, LoaderComponentId, LoaderVersionIndex},
};
use crate::paths::{loader_catalog_dir, versions_dir};
use crate::types::VersionEntry;
use crate::version_meta::{analyze_version_metadata, compare_version_entries};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::Path;

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

pub fn scan_versions(mc_dir: &Path) -> std::io::Result<Vec<VersionEntry>> {
    let versions_dir = versions_dir(mc_dir);
    let entries = fs::read_dir(&versions_dir)?;
    let mut stubs = HashMap::new();

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
        stubs.insert(id, stub);
    }

    let mut versions = Vec::new();
    for (id, stub) in &stubs {
        let effective_parent = effective_parent_version(id, &stub.inherits_from);
        let jar_path = versions_dir.join(id).join(format!("{id}.jar"));
        let incomplete_marker = versions_dir.join(id).join(".incomplete");

        let resolved_java = resolve_java_version(id, &stubs);
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

        let loader_meta = infer_build_from_version_id(id);
        let loader_prerelease = infer_loader_prerelease(mc_dir, loader_meta.as_ref());
        let metadata = analyze_version_metadata(id, &stub.kind, &stub.release_time, None, &[]);

        versions.push(VersionEntry {
            id: id.clone(),
            kind: metadata.canonical_kind.clone(),
            release_time: stub.release_time.clone(),
            meta: metadata,
            inherits_from: effective_parent.clone(),
            launchable,
            installed: true,
            status,
            status_detail,
            needs_install,
            java_component: resolved_java.component,
            java_major: resolved_java.major_version,
            manifest_url: String::new(),
            loader_component_id: loader_meta
                .as_ref()
                .map(|(component_id, _, _, _)| component_id.as_str().to_string()),
            loader_build_id: loader_meta.map(|(_, build_id, _, _)| build_id),
            loader_prerelease,
        });
    }

    versions.sort_by(compare_version_entries);
    Ok(versions)
}

fn resolve_java_version(id: &str, stubs: &HashMap<String, VersionStub>) -> JavaVersionStub {
    let mut current_id = id.to_string();
    let mut current = stubs.get(&current_id);
    let mut fallback_parent = String::new();
    while let Some(stub) = current {
        if let Some(java_version) = &stub.java_version {
            return java_version.clone();
        }
        let next_parent = effective_parent_version(&current_id, &stub.inherits_from);
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

fn effective_parent_version(id: &str, declared_parent: &str) -> String {
    if !declared_parent.trim().is_empty() {
        return declared_parent.to_string();
    }
    infer_loader_base_version(id).unwrap_or_default()
}

fn infer_loader_base_version(id: &str) -> Option<String> {
    let lower = id.to_ascii_lowercase();

    if let Some(rest) = id.strip_prefix("fabric-loader-") {
        return rest.rsplit_once('-').map(|(_, base)| base.to_string());
    }
    if let Some(rest) = id.strip_prefix("quilt-loader-") {
        return rest.rsplit_once('-').map(|(_, base)| base.to_string());
    }
    if lower.starts_with("neoforge-") {
        let version = id.strip_prefix("neoforge-")?;
        return Some(neoforge_to_mc_version(version));
    }
    id.split_once("-forge-").map(|(base, _)| base.to_string())
}

fn neoforge_to_mc_version(version: &str) -> String {
    let numeric_parts = version
        .split('.')
        .map(|part| {
            part.chars()
                .take_while(|ch| ch.is_ascii_digit())
                .collect::<String>()
        })
        .take_while(|part| !part.is_empty())
        .collect::<Vec<_>>();
    let Some(major) = numeric_parts.first() else {
        return String::new();
    };
    let Some(minor) = numeric_parts.get(1) else {
        return String::new();
    };

    if major.parse::<u32>().ok().is_some_and(|value| value >= 25) {
        let mut parts = vec![major.clone(), minor.clone()];
        if let Some(patch) = numeric_parts.get(2)
            && patch != "0"
        {
            parts.push(patch.clone());
        }
        return parts.join(".");
    }

    if minor == "0" {
        return format!("1.{major}");
    }

    format!("1.{major}.{minor}")
}

fn infer_loader_prerelease(
    mc_dir: &Path,
    loader_meta: Option<&(LoaderComponentId, String, String, String)>,
) -> Option<bool> {
    let (component_id, build_id, minecraft_version, loader_version) = loader_meta?;
    if is_prerelease_loader_version(loader_version) {
        return Some(true);
    }

    let cache_path = loader_catalog_dir(mc_dir).join(format!(
        "component-{}-builds-{}.json",
        component_id.short_key(),
        minecraft_version
    ));
    let data = fs::read(cache_path).ok()?;
    let cached = serde_json::from_slice::<CachedCatalog<LoaderVersionIndex>>(&data).ok()?;
    cached
        .value
        .builds
        .into_iter()
        .find(|build| build.build_id == *build_id)
        .map(|build| build.prerelease)
}

fn is_prerelease_loader_version(loader_version: &str) -> bool {
    let lower = loader_version.to_ascii_lowercase();
    ["alpha", "beta", "snapshot", "pre", "rc"]
        .into_iter()
        .any(|marker| lower.contains(marker))
}

#[cfg(test)]
mod tests {
    use super::{JavaVersionStub, VersionStub, resolve_java_version, scan_versions};
    use crate::loaders::types::{
        CachedCatalog, LoaderArtifactKind, LoaderBuildRecord, LoaderComponentId,
        LoaderInstallSource, LoaderInstallStrategy, LoaderInstallability, LoaderVersionIndex,
    };
    use std::collections::HashMap;
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn resolve_java_version_follows_current_parent_chain_for_loader_versions() {
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

        let resolved = resolve_java_version("fabric-loader-0.14.21-1.20.1", &stubs);

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
    fn scan_versions_reads_loader_prerelease_from_cached_build_index() {
        let mc_dir = unique_test_dir("loader-prerelease-cache");
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

        let cache_dir = mc_dir.join("cache").join("loaders").join("catalog");
        fs::create_dir_all(&cache_dir).expect("create cache dir");
        let cache = CachedCatalog::new(LoaderVersionIndex {
            component_id: LoaderComponentId::Forge,
            builds: vec![LoaderBuildRecord {
                component_id: LoaderComponentId::Forge,
                component_name: "Forge".to_string(),
                build_id: "forge:26.1.2:64.0.4".to_string(),
                minecraft_version: "26.1.2".to_string(),
                loader_version: "64.0.4".to_string(),
                version_id: "26.1.2-forge-64.0.4".to_string(),
                stable: false,
                prerelease: true,
                recommended: false,
                latest: true,
                strategy: LoaderInstallStrategy::ForgeModern,
                artifact_kind: LoaderArtifactKind::InstallerJar,
                installability: LoaderInstallability::Installable,
                install_source: LoaderInstallSource::InstallerJar {
                    url: "https://example.invalid/forge-installer.jar".to_string(),
                },
            }],
        });
        fs::write(
            cache_dir.join("component-forge-builds-26.1.2.json"),
            serde_json::to_vec_pretty(&cache).expect("serialize cache"),
        )
        .expect("write cache");

        let versions = scan_versions(&mc_dir).expect("scan versions");
        let version = versions
            .iter()
            .find(|entry| entry.id == "26.1.2-forge-64.0.4")
            .expect("forge version exists");

        assert_eq!(version.loader_prerelease, Some(true));

        fs::remove_dir_all(&mc_dir).expect("remove temp test dir");
    }

    fn unique_test_dir(name: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time ok")
            .as_nanos();
        std::env::temp_dir().join(format!("croopor-{name}-{unique}"))
    }
}
