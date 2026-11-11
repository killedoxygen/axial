//! What a mod jar's own loader metadata says it needs. Provider metadata cannot
//! express loader-version floors or exact mod-id pins, so content installs read
//! them from the staged jars before publication. Anything this module cannot
//! parse is treated as unknown and never refuses an install.

use axial_minecraft::loaders::LoaderComponentId;
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::io::{Cursor, Read};

const MAX_METADATA_BYTES: u64 = 1 << 20;
const MAX_NESTED_JAR_BYTES: u64 = 64 << 20;
const MAX_NESTED_DEPTH: usize = 3;
const MAX_NESTED_JARS: usize = 4096;
/// Ids the loader supplies itself rather than a jar in `mods/`.
const BUILTIN_IDS: [&str; 5] = [
    "java",
    "minecraft",
    "fabricloader",
    "fabric-loader",
    "mixinextras",
];

/// The game a mod joins: loader component, loader version and Minecraft version.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Target {
    loader: LoaderComponentId,
    loader_version: String,
    minecraft: String,
}

impl Target {
    pub(crate) fn of(version_id: &str) -> Option<Self> {
        let identity =
            axial_minecraft::loaders::api::decode_installed_version_id(version_id).ok()?;
        let minecraft = identity.minecraft_version().to_string();
        let loader_version = identity.loader_version();
        let loader_version = loader_version
            .strip_prefix(&format!("{minecraft}-"))
            .unwrap_or(loader_version)
            .to_string();
        Some(Self {
            loader: identity.component_id(),
            loader_version,
            minecraft,
        })
    }

    /// Quilt reinterprets Fabric metadata, so only loaders whose own metadata
    /// format is read here are checked.
    pub(crate) fn is_checked(&self) -> bool {
        self.loader != LoaderComponentId::Quilt
    }
}

/// One jar's declarations, including the ids of jars nested inside it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Declared {
    name: Option<String>,
    provided: Vec<(String, Option<String>)>,
    depends: Vec<(String, Vec<String>)>,
    breaks: Vec<(String, Vec<String>)>,
    client: bool,
    /// Every nested jar was read.
    complete: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Jar {
    Mod(Declared),
    /// The loader ignores jars without its metadata.
    NotAMod,
    Unreadable,
}

impl Jar {
    pub(crate) fn read(target: &Target, bytes: &[u8]) -> Self {
        let Ok(mut archive) = zip::ZipArchive::new(Cursor::new(bytes)) else {
            return Self::Unreadable;
        };
        let name = match target.loader {
            LoaderComponentId::Fabric => "fabric.mod.json",
            LoaderComponentId::NeoForge => "META-INF/neoforge.mods.toml",
            LoaderComponentId::Forge => "META-INF/mods.toml",
            LoaderComponentId::Quilt => return Self::Unreadable,
        };
        if archive.index_for_name(name).is_none() {
            return Self::NotAMod;
        }
        let mut budget = MAX_NESTED_JARS;
        let declared = match target.loader {
            LoaderComponentId::Fabric => fabric(&mut archive, 0, &mut budget),
            _ => forge(&mut archive, name),
        };
        declared.map_or(Self::Unreadable, Self::Mod)
    }
}

/// The mods that stay in the instance, the ids they provide and their own
/// declarations. A partial inventory still checks versions but never reports
/// a missing id.
#[derive(Debug, Default)]
pub(crate) struct Inventory {
    provided: HashMap<String, Vec<Option<String>>>,
    mods: Vec<Declared>,
    complete: bool,
}

impl Inventory {
    pub(crate) fn new() -> Self {
        Self {
            complete: true,
            ..Self::default()
        }
    }

    pub(crate) fn add(&mut self, jar: Jar) {
        match jar {
            Jar::Mod(declared) => {
                self.provide(&declared);
                self.mods.push(declared);
            }
            Jar::NotAMod => {}
            Jar::Unreadable => self.complete = false,
        }
    }

    fn provide(&mut self, declared: &Declared) {
        self.complete &= declared.complete;
        for (id, version) in &declared.provided {
            self.provided
                .entry(id.clone())
                .or_default()
                .push(version.clone());
        }
    }

    pub(crate) fn mark_incomplete(&mut self) {
        self.complete = false;
    }
}

/// Player-readable reasons the candidates would stop the game from loading:
/// their own needs and conflicts, and those of staying mods they affect.
pub(crate) fn refusals(
    target: &Target,
    candidates: &[(String, Declared)],
    installed: &Inventory,
) -> Vec<String> {
    let mut inventory = Inventory {
        provided: installed.provided.clone(),
        mods: Vec::new(),
        complete: installed.complete,
    };
    let mut incoming = HashSet::new();
    for (_, declared) in candidates {
        inventory.provide(declared);
        incoming.extend(declared.provided.iter().map(|(id, _)| id.as_str()));
    }
    let declarers = candidates
        .iter()
        .map(|(title, declared)| (title.as_str(), declared, true))
        .chain(installed.mods.iter().map(|declared| {
            let title = declared.provided.first().map_or("", |(id, _)| id.as_str());
            (title, declared, false)
        }));
    let mut reasons = Vec::new();
    for (title, declared, candidate) in declarers {
        if !declared.client {
            continue;
        }
        let title = declared.name.as_deref().unwrap_or(title);
        let has = if candidate {
            "This instance has"
        } else {
            "The update brings"
        };
        for (id, predicates) in &declared.depends {
            if !candidate && !incoming.contains(id.as_str()) {
                continue;
            }
            let (label, versions): (&str, Vec<Option<String>>) = match id.as_str() {
                "java" => continue,
                "minecraft" => ("Minecraft", vec![Some(target.minecraft.clone())]),
                "fabricloader" if target.loader == LoaderComponentId::Fabric => {
                    ("Fabric Loader", vec![Some(target.loader_version.clone())])
                }
                "neoforge" if target.loader == LoaderComponentId::NeoForge => {
                    ("NeoForge", vec![Some(target.loader_version.clone())])
                }
                "forge" if target.loader == LoaderComponentId::Forge => {
                    ("Forge", vec![Some(target.loader_version.clone())])
                }
                _ if target.loader != LoaderComponentId::Fabric => continue,
                other => match inventory.provided.get(other) {
                    Some(versions) => (other, versions.clone()),
                    None => {
                        if inventory.complete && !BUILTIN_IDS.contains(&other) {
                            reasons.push(format!(
                                "{title} needs {other}{}, which isn't installed.",
                                wanted(predicates)
                                    .map(|v| format!(" {v}"))
                                    .unwrap_or_default()
                            ));
                        }
                        continue;
                    }
                },
            };
            let verdicts: Vec<Option<bool>> = versions
                .iter()
                .map(|version| {
                    version
                        .as_deref()
                        .and_then(|version| satisfies(target.loader, version, predicates))
                })
                .collect();
            if verdicts.iter().all(|verdict| *verdict == Some(false)) {
                let present = versions
                    .iter()
                    .flatten()
                    .map(String::as_str)
                    .collect::<Vec<_>>()
                    .join(", ");
                reasons.push(format!(
                    "{title} needs {label} {}. {has} {present}.",
                    wanted(predicates).unwrap_or_else(|| predicates.join(" or "))
                ));
            }
        }
        for (id, predicates) in &declared.breaks {
            if !candidate && !incoming.contains(id.as_str()) {
                continue;
            }
            for version in inventory.provided.get(id).into_iter().flatten().flatten() {
                if satisfies(target.loader, version, predicates) == Some(true) {
                    reasons.push(format!("{title} can't be used with {id} {version}."));
                }
            }
        }
    }
    reasons.sort();
    reasons.dedup();
    reasons
}

fn wanted(predicates: &[String]) -> Option<String> {
    let [predicate] = predicates else {
        return None;
    };
    let predicate = predicate.trim();
    if predicate == "*" || predicate.is_empty() {
        return None;
    }
    if let Some(version) = predicate.strip_prefix(">=")
        && !version.contains(' ')
    {
        return Some(format!("{version} or newer"));
    }
    if let Some(range) = predicate.strip_prefix('[')
        && let Some(version) = range.strip_suffix(",)")
    {
        return Some(format!("{version} or newer"));
    }
    Some(predicate.trim_start_matches('=').to_string())
}

fn read_entry(
    archive: &mut zip::ZipArchive<Cursor<&[u8]>>,
    name: &str,
    limit: u64,
) -> Option<Vec<u8>> {
    let entry = archive.by_name(name).ok()?;
    if entry.size() > limit {
        return None;
    }
    let mut bytes = Vec::new();
    entry.take(limit).read_to_end(&mut bytes).ok()?;
    Some(bytes)
}

fn fabric(
    archive: &mut zip::ZipArchive<Cursor<&[u8]>>,
    depth: usize,
    budget: &mut usize,
) -> Option<Declared> {
    let metadata = read_entry(archive, "fabric.mod.json", MAX_METADATA_BYTES)?;
    let json: serde_json::Value = serde_json::from_slice(&metadata).ok()?;
    let id = json.get("id")?.as_str()?.to_string();
    let version = json
        .get("version")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let mut declared = Declared {
        name: json
            .get("name")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        provided: vec![(id, version.clone())],
        depends: Vec::new(),
        breaks: Vec::new(),
        client: match json.get("environment") {
            None => true,
            Some(serde_json::Value::String(environment)) => environment != "server",
            Some(_) => true,
        },
        complete: true,
    };
    for provided in json
        .get("provides")
        .and_then(|v| v.as_array())
        .into_iter()
        .flatten()
        .filter_map(|v| v.as_str())
    {
        declared
            .provided
            .push((provided.to_string(), version.clone()));
    }
    for (field, into) in [
        ("depends", &mut declared.depends),
        ("breaks", &mut declared.breaks),
    ] {
        for (id, predicate) in json
            .get(field)
            .and_then(|v| v.as_object())
            .into_iter()
            .flatten()
        {
            let predicates = match predicate {
                serde_json::Value::String(value) => vec![value.clone()],
                serde_json::Value::Array(values) => values
                    .iter()
                    .map(|value| value.as_str().map(str::to_string))
                    .collect::<Option<Vec<_>>>()?,
                _ => return None,
            };
            into.push((id.clone(), predicates));
        }
    }
    for file in json
        .get("jars")
        .and_then(|v| v.as_array())
        .into_iter()
        .flatten()
        .filter_map(|jar| jar.get("file").and_then(|v| v.as_str()))
    {
        let nested = if depth < MAX_NESTED_DEPTH && *budget > 0 {
            *budget -= 1;
            read_entry(archive, file, MAX_NESTED_JAR_BYTES).and_then(|bytes| {
                let mut nested = zip::ZipArchive::new(Cursor::new(bytes.as_slice())).ok()?;
                fabric(&mut nested, depth + 1, budget)
            })
        } else {
            None
        };
        match nested {
            Some(nested) => {
                declared.complete &= nested.complete;
                declared.provided.extend(nested.provided);
            }
            None => declared.complete = false,
        }
    }
    Some(declared)
}

fn forge(archive: &mut zip::ZipArchive<Cursor<&[u8]>>, name: &str) -> Option<Declared> {
    let metadata = read_entry(archive, name, MAX_METADATA_BYTES)?;
    let table: toml::Table = std::str::from_utf8(&metadata).ok()?.parse().ok()?;
    let mut declared = Declared {
        name: table
            .get("mods")
            .and_then(|mods| mods.as_array())
            .and_then(|mods| mods.first())
            .and_then(|first| first.get("displayName"))
            .and_then(|name| name.as_str())
            .map(str::to_string),
        client: true,
        complete: true,
        ..Declared::default()
    };
    for dependency in table
        .get("dependencies")
        .and_then(|v| v.as_table())
        .into_iter()
        .flat_map(|by_mod| by_mod.values())
        .filter_map(|v| v.as_array())
        .flatten()
    {
        let Some(id) = dependency.get("modId").and_then(|v| v.as_str()) else {
            continue;
        };
        let required = match dependency.get("type").and_then(|v| v.as_str()) {
            Some(kind) => kind.eq_ignore_ascii_case("required"),
            None => dependency
                .get("mandatory")
                .and_then(|v| v.as_bool())
                .unwrap_or(true),
        };
        let client = dependency
            .get("side")
            .and_then(|v| v.as_str())
            .is_none_or(|side| !side.eq_ignore_ascii_case("server"));
        let Some(range) = dependency.get("versionRange").and_then(|v| v.as_str()) else {
            continue;
        };
        if required && client {
            declared
                .depends
                .push((id.to_string(), vec![range.to_string()]));
        }
    }
    Some(declared)
}

/// `None` when either side cannot be read; unknown is never a refusal.
fn satisfies(loader: LoaderComponentId, version: &str, predicates: &[String]) -> Option<bool> {
    let version = Version::parse(version, false)?;
    let mut any_unknown = false;
    for predicate in predicates {
        let verdict = if loader == LoaderComponentId::Fabric {
            fabric_predicate(&version, predicate)
        } else {
            maven_range(&version, predicate)
        };
        match verdict {
            Some(true) => return Some(true),
            Some(false) => {}
            None => any_unknown = true,
        }
    }
    (!any_unknown).then_some(false)
}

fn fabric_predicate(version: &Version, predicate: &str) -> Option<bool> {
    let mut result = true;
    let terms: Vec<&str> = predicate.split_whitespace().collect();
    if terms.is_empty() {
        return Some(true);
    }
    for term in terms {
        if term == "*" {
            continue;
        }
        let (operator, raw) = [">=", "<=", ">", "<", "=", "^", "~"]
            .into_iter()
            .find_map(|operator| term.strip_prefix(operator).map(|rest| (operator, rest)))
            .unwrap_or(("=", term));
        let wanted = Version::parse(raw, operator == "=")?;
        let ordering = version.compare(&wanted);
        result &= match operator {
            "=" if wanted.wildcard().is_some() => version.matches_prefix(&wanted),
            "=" => ordering == Ordering::Equal,
            ">=" => ordering != Ordering::Less,
            "<=" => ordering != Ordering::Greater,
            ">" => ordering == Ordering::Greater,
            "<" => ordering == Ordering::Less,
            "^" => ordering != Ordering::Less && version.part(0) == wanted.part(0),
            "~" => {
                ordering != Ordering::Less
                    && version.part(0) == wanted.part(0)
                    && version.part(1) == wanted.part(1)
            }
            _ => return None,
        };
    }
    Some(result)
}

fn maven_range(version: &Version, range: &str) -> Option<bool> {
    let range = range.trim();
    if range.is_empty() || range == "*" {
        return Some(true);
    }
    if !range.starts_with(['[', '(']) {
        // A bare Maven version is a soft recommendation, not a requirement.
        return Version::parse(range, false).map(|_| true);
    }
    let mut rest = range;
    let mut any = false;
    while !rest.is_empty() {
        let open = rest.chars().next()?;
        let close_at = rest.find([']', ')'])?;
        let close = rest[close_at..].chars().next()?;
        let body = &rest[1..close_at];
        any |= match body.split_once(',') {
            None => {
                open == '['
                    && close == ']'
                    && version.compare(&Version::parse(body.trim(), false)?) == Ordering::Equal
            }
            Some((low, high)) => {
                let low_ok = match low.trim() {
                    "" => true,
                    low => {
                        let ordering = version.compare(&Version::parse(low, false)?);
                        ordering == Ordering::Greater
                            || (open == '[' && ordering == Ordering::Equal)
                    }
                };
                let high_ok = match high.trim() {
                    "" => true,
                    high => {
                        let ordering = version.compare(&Version::parse(high, false)?);
                        ordering == Ordering::Less || (close == ']' && ordering == Ordering::Equal)
                    }
                };
                low_ok && high_ok
            }
        };
        rest = rest[close_at + 1..].trim_start_matches([',', ' ']);
    }
    Some(any)
}

/// Numeric release components with an optional pre-release, build metadata
/// ignored, matching how Fabric and Maven order mod versions.
#[derive(Debug, PartialEq, Eq)]
struct Version {
    parts: Vec<Option<u64>>,
    pre: Option<Vec<String>>,
}

impl Version {
    fn parse(raw: &str, wildcards: bool) -> Option<Self> {
        let raw = raw.trim();
        let core = raw.split_once('+').map_or(raw, |(core, _)| core);
        let (release, pre) = match core.split_once('-') {
            Some((release, pre)) => (
                release,
                Some(if pre.is_empty() {
                    Vec::new()
                } else {
                    pre.split('.').map(str::to_string).collect()
                }),
            ),
            None => (core, None),
        };
        let mut parts = Vec::new();
        for part in release.split('.') {
            match part {
                "x" | "X" | "*" if wildcards && !parts.is_empty() => parts.push(None),
                _ if parts.last() == Some(&None) => return None,
                _ => parts.push(Some(part.parse().ok()?)),
            }
        }
        if parts.is_empty() || parts.len() > 8 {
            return None;
        }
        Some(Self { parts, pre })
    }

    fn part(&self, index: usize) -> u64 {
        self.parts.get(index).copied().flatten().unwrap_or(0)
    }

    fn wildcard(&self) -> Option<usize> {
        self.parts.iter().position(Option::is_none)
    }

    fn matches_prefix(&self, wanted: &Self) -> bool {
        let fixed = wanted.wildcard().unwrap_or(wanted.parts.len());
        (0..fixed).all(|index| self.part(index) == wanted.part(index))
    }

    fn compare(&self, other: &Self) -> Ordering {
        let len = self.parts.len().max(other.parts.len());
        for index in 0..len {
            match self.part(index).cmp(&other.part(index)) {
                Ordering::Equal => {}
                ordering => return ordering,
            }
        }
        match (&self.pre, &other.pre) {
            (None, None) => Ordering::Equal,
            (None, Some(_)) => Ordering::Greater,
            (Some(_), None) => Ordering::Less,
            (Some(left), Some(right)) => {
                for (left, right) in left.iter().zip(right) {
                    let ordering = match (left.parse::<u64>(), right.parse::<u64>()) {
                        (Ok(left), Ok(right)) => left.cmp(&right),
                        (Ok(_), Err(_)) => Ordering::Less,
                        (Err(_), Ok(_)) => Ordering::Greater,
                        (Err(_), Err(_)) => left.cmp(right),
                    };
                    if ordering != Ordering::Equal {
                        return ordering;
                    }
                }
                left.len().cmp(&right.len())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn jar(entries: &[(&str, Vec<u8>)]) -> Vec<u8> {
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        for (name, bytes) in entries {
            writer
                .start_file(*name, zip::write::SimpleFileOptions::default())
                .unwrap();
            writer.write_all(bytes).unwrap();
        }
        writer.finish().unwrap().into_inner()
    }

    fn fabric_jar(metadata: serde_json::Value, nested: &[(&str, Vec<u8>)]) -> Vec<u8> {
        let mut entries = vec![("fabric.mod.json", serde_json::to_vec(&metadata).unwrap())];
        entries.extend(nested.iter().map(|(name, bytes)| (*name, bytes.clone())));
        jar(&entries)
    }

    fn fabric_target() -> Target {
        Target {
            loader: LoaderComponentId::Fabric,
            loader_version: "0.19.3".into(),
            minecraft: "1.21.1".into(),
        }
    }

    #[test]
    fn predicates_follow_fabric_ordering_and_maven_interval_notation() {
        use LoaderComponentId::{Fabric, NeoForge};
        let cases = [
            (Fabric, "0.19.3", ">=0.20.1", Some(false)),
            (Fabric, "0.20.1", ">=0.20.1", Some(true)),
            (Fabric, "1.21.1", ">=1.21- <1.21.2-", Some(true)),
            (Fabric, "1.21.2", ">=1.21- <1.21.2-", Some(false)),
            (Fabric, "1.21.1", "~1.21 <1.21.2", Some(true)),
            (Fabric, "0.8.13+mc1.21.1", "0.8.x", Some(true)),
            (Fabric, "0.7.4+mc1.21", "=0.8.13+mc1.21.1", Some(false)),
            (Fabric, "0.8.13+mc1.21.1", "0.8.13+mc1.21.1", Some(true)),
            (Fabric, "1.0.0-beta.5+mc1.21.1", "1.0.0-beta.5+mc1.21.1", Some(true)),
            (Fabric, "1.0.0-beta.4", ">=1.0.0-beta.5", Some(false)),
            (Fabric, "1.21.1", "*", Some(true)),
            (Fabric, "24w14a", ">=1.21", None),
            (Fabric, "1.21.1", ">=banana", None),
            (NeoForge, "21.1.77", "[21.1.77,)", Some(true)),
            (NeoForge, "21.1.76", "[21.1.77,)", Some(false)),
            (NeoForge, "1.21.1", "[1.21.1,1.21.2)", Some(true)),
            (NeoForge, "1.21.2", "[1.21.1,1.21.2)", Some(false)),
            (NeoForge, "1.21.1", "[1.21.1]", Some(true)),
            (NeoForge, "1.21", "[1.20,1.20.6],[1.21,)", Some(true)),
            (NeoForge, "21.1.77", "*", Some(true)),
            (NeoForge, "21.1.77", "21.1.200", Some(true)),
        ];
        for (loader, version, predicate, expected) in cases {
            assert_eq!(
                satisfies(loader, version, &[predicate.to_string()]),
                expected,
                "{version} {predicate}"
            );
        }
        let any_of = ["1.21".to_string(), "1.21.1".to_string()];
        assert_eq!(satisfies(Fabric, "1.21.1", &any_of), Some(true));
    }

    #[test]
    fn refusals_name_loader_floors_and_missing_or_mismatched_mods() {
        let target = fabric_target();
        let nested = fabric_jar(
            serde_json::json!({"id":"fabric-resource-loader-v0","version":"1.0.0"}),
            &[],
        );
        let api = fabric_jar(
            serde_json::json!({"id":"fabric-api","version":"0.116.17+1.21.1","provides":["fabric"],
                "jars":[{"file":"META-INF/jars/loader.jar"}]}),
            &[("META-INF/jars/loader.jar", nested)],
        );
        let skyboxes = fabric_jar(
            serde_json::json!({"id":"fabricskyboxes","version":"0.7.4+mc1.21"}),
            &[],
        );
        let mut installed = Inventory::new();
        for bytes in [
            &api,
            &skyboxes,
            &jar(&[("readme.txt", b"library".to_vec())]),
        ] {
            installed.add(Jar::read(&target, bytes));
        }
        let candidate = |metadata| match Jar::read(&target, &fabric_jar(metadata, &[])) {
            Jar::Mod(declared) => declared,
            other => panic!("{other:?}"),
        };
        let extra = candidate(
            serde_json::json!({"id":"sodium-extra","name":"Sodium Extra","version":"0.9.5",
            "depends":{"fabricloader":">=0.20.1","minecraft":"1.21.1","fabric-resource-loader-v0":"*","fabric":"*"}}),
        );
        let interop = candidate(
            serde_json::json!({"id":"nuit_interop","name":"Nuit Interop","version":"1.0.0",
            "depends":{"fabricloader":">=0.15.11","nuit":"1.0.0-beta.5+mc1.21.1","java":">=21"}}),
        );
        let culling = candidate(
            serde_json::json!({"id":"culling","name":"Culling","version":"1.0.0",
            "depends":{"fabricskyboxes":">=0.8"}}),
        );
        let server = candidate(
            serde_json::json!({"id":"server","version":"1.0.0","environment":"server",
            "depends":{"fabricloader":">=9"}}),
        );
        let refused = refusals(
            &target,
            &[
                ("Sodium Extra".into(), extra.clone()),
                ("Nuit Interop".into(), interop),
                ("Culling".into(), culling),
                ("Server".into(), server),
            ],
            &installed,
        );
        assert_eq!(
            refused,
            [
                "Culling needs fabricskyboxes 0.8 or newer. This instance has 0.7.4+mc1.21.",
                "Nuit Interop needs nuit 1.0.0-beta.5+mc1.21.1, which isn't installed.",
                "Sodium Extra needs Fabric Loader 0.20.1 or newer. This instance has 0.19.3.",
            ]
        );

        let mut partial = Inventory::new();
        partial.mark_incomplete();
        let newer = Target {
            loader_version: "0.20.1".into(),
            ..target.clone()
        };
        let interop = candidate(serde_json::json!({"id":"nuit_interop","version":"1.0.0",
            "depends":{"nuit":"1.0.0-beta.5"}}));
        assert!(
            refusals(
                &newer,
                &[
                    ("Sodium Extra".into(), extra),
                    ("Nuit Interop".into(), interop)
                ],
                &partial
            )
            .is_empty()
        );
    }

    #[test]
    fn refusals_check_breaks_and_staying_mods_the_batch_affects() {
        let target = fabric_target();
        let read = |metadata| match Jar::read(&target, &fabric_jar(metadata, &[])) {
            Jar::Mod(declared) => declared,
            other => panic!("{other:?}"),
        };
        let mut installed = Inventory::new();
        for metadata in [
            serde_json::json!({"id":"sodium-extra","name":"Sodium Extra","version":"0.6.0+mc1.21.1"}),
            serde_json::json!({"id":"iris","name":"Iris","version":"1.8.14","depends":{"sodium":"0.8.x"}}),
            serde_json::json!({"id":"unrelated","name":"Unrelated","version":"1.0.0","depends":{"gone":"*"}}),
        ] {
            installed.add(Jar::Mod(read(metadata)));
        }
        let sodium = |version: &str| {
            read(
                serde_json::json!({"id":"sodium","name":"Sodium","version":version,
                "breaks":{"sodium-extra":"<0.8.7"}}),
            )
        };
        assert_eq!(
            refusals(&target, &[("Sodium".into(), sodium("0.9.0"))], &installed),
            [
                "Iris needs sodium 0.8.x. The update brings 0.9.0.",
                "Sodium can't be used with sodium-extra 0.6.0+mc1.21.1.",
            ]
        );
        let extra = read(serde_json::json!({"id":"sodium-extra","version":"0.8.7"}));
        let mut replaced = Inventory::new();
        replaced.add(Jar::Mod(read(
            serde_json::json!({"id":"iris","version":"1.8.14","depends":{"sodium":"0.8.x"}}),
        )));
        assert!(
            refusals(
                &target,
                &[
                    ("Sodium".into(), sodium("0.8.13")),
                    ("Sodium Extra".into(), extra)
                ],
                &replaced
            )
            .is_empty()
        );
    }

    #[test]
    fn neoforge_reads_required_client_loader_ranges() {
        let target = Target {
            loader: LoaderComponentId::NeoForge,
            loader_version: "21.1.77".into(),
            minecraft: "1.21.1".into(),
        };
        let toml = br#"
modLoader="javafml"
[[mods]]
modId="example"
displayName="Example"
[[dependencies.example]]
modId="neoforge"
type="required"
versionRange="[21.1.90,)"
side="BOTH"
[[dependencies.example]]
modId="minecraft"
type="required"
versionRange="[1.21.1,1.21.2)"
[[dependencies.example]]
modId="jei"
type="optional"
versionRange="[99,)"
"#;
        let Jar::Mod(declared) = Jar::read(
            &target,
            &jar(&[("META-INF/neoforge.mods.toml", toml.to_vec())]),
        ) else {
            panic!("unreadable neoforge metadata");
        };
        assert_eq!(
            refusals(
                &target,
                &[("example.jar".into(), declared)],
                &Inventory::new()
            ),
            ["Example needs NeoForge 21.1.90 or newer. This instance has 21.1.77."]
        );
        assert_eq!(Jar::read(&target, b"not a jar"), Jar::Unreadable);
    }
}
