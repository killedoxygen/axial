use crate::loaders::infer_build_from_version_id;
use crate::paths::versions_dir;
use crate::types::VersionEntry;
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
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
                    id.clone(),
                )
            } else if !parent_jar.is_file() {
                (
                    false,
                    "incomplete".to_string(),
                    format!("Base version {} needs to be downloaded", effective_parent),
                    id.clone(),
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

        versions.push(VersionEntry {
            id: id.clone(),
            kind: stub.kind.clone(),
            release_time: stub.release_time.clone(),
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
        });
    }

    versions.sort_by(compare_version_entries);
    Ok(versions)
}

fn resolve_java_version(id: &str, stubs: &HashMap<String, VersionStub>) -> JavaVersionStub {
    let mut current = stubs.get(id);
    let mut fallback_parent = String::new();
    while let Some(stub) = current {
        if let Some(java_version) = &stub.java_version {
            return java_version.clone();
        }
        let next_parent = effective_parent_version(id, &stub.inherits_from);
        if next_parent.is_empty() {
            break;
        }
        fallback_parent = next_parent.clone();
        current = stubs.get(&next_parent);
    }

    if !fallback_parent.is_empty() && fallback_parent != id
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

fn compare_version_entries(left: &VersionEntry, right: &VersionEntry) -> Ordering {
    let left_priority = version_type_priority(&left.kind);
    let right_priority = version_type_priority(&right.kind);
    left_priority
        .cmp(&right_priority)
        .then_with(|| compare_version_ids(&right.id, &left.id))
}

fn version_type_priority(kind: &str) -> i32 {
    match kind {
        "release" => 0,
        "snapshot" => 1,
        "old_beta" => 2,
        "old_alpha" => 3,
        _ => 4,
    }
}

fn compare_version_ids(left: &str, right: &str) -> Ordering {
    let left_parts = split_version_parts(left);
    let right_parts = split_version_parts(right);
    let len = left_parts.len().max(right_parts.len());

    for index in 0..len {
        let left_part = left_parts.get(index).map(String::as_str).unwrap_or("");
        let right_part = right_parts.get(index).map(String::as_str).unwrap_or("");

        match (left_part.parse::<i32>(), right_part.parse::<i32>()) {
            (Ok(left_num), Ok(right_num)) if left_num != right_num => {
                return left_num.cmp(&right_num);
            }
            _ if left_part != right_part => return left_part.cmp(right_part),
            _ => {}
        }
    }

    Ordering::Equal
}

fn split_version_parts(version: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();

    for ch in version.chars() {
        if ch == '.' || ch == '-' {
            if !current.is_empty() {
                parts.push(std::mem::take(&mut current));
            }
        } else {
            current.push(ch);
        }
    }

    if !current.is_empty() {
        parts.push(current);
    }

    parts
}
