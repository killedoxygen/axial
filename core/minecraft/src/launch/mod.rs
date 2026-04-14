use crate::paths::{libraries_dir, versions_dir};
use crate::rules::{Environment, Rule, evaluate_rules, is_native_library};
use serde::{Deserialize, Deserializer, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionJson {
    pub id: String,
    #[serde(rename = "inheritsFrom", default)]
    pub inherits_from: String,
    #[serde(rename = "type", default)]
    pub kind: String,
    #[serde(rename = "mainClass", default)]
    pub main_class: String,
    #[serde(rename = "minimumLauncherVersion", default)]
    pub minimum_launcher_version: i32,
    #[serde(rename = "complianceLevel", default)]
    pub compliance_level: i32,
    #[serde(rename = "releaseTime", default)]
    pub release_time: String,
    #[serde(default)]
    pub time: String,
    #[serde(default)]
    pub arguments: Option<ArgumentsSection>,
    #[serde(rename = "minecraftArguments", default)]
    pub minecraft_arguments: String,
    #[serde(rename = "assetIndex")]
    pub asset_index: AssetIndex,
    #[serde(default)]
    pub assets: String,
    #[serde(default)]
    pub downloads: Downloads,
    #[serde(rename = "javaVersion", default)]
    pub java_version: JavaVersion,
    #[serde(default)]
    pub libraries: Vec<Library>,
    #[serde(default)]
    pub logging: Option<LoggingConf>,
}

impl VersionJson {
    pub fn is_legacy_version(&self) -> bool {
        self.arguments.is_none() && !self.minecraft_arguments.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ArgumentsSection {
    #[serde(default)]
    pub game: Vec<Argument>,
    #[serde(default)]
    pub jvm: Vec<Argument>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Default)]
pub struct Argument {
    pub rules: Vec<Rule>,
    pub value: Vec<String>,
}

impl<'de> Deserialize<'de> for Argument {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum RawArgument {
            Plain(String),
            Conditional {
                #[serde(default)]
                rules: Vec<Rule>,
                value: RawArgumentValue,
            },
        }

        #[derive(Deserialize)]
        #[serde(untagged)]
        enum RawArgumentValue {
            Single(String),
            Many(Vec<String>),
        }

        let raw = RawArgument::deserialize(deserializer)?;
        Ok(match raw {
            RawArgument::Plain(value) => Self {
                rules: Vec::new(),
                value: vec![value],
            },
            RawArgument::Conditional { rules, value } => Self {
                rules,
                value: match value {
                    RawArgumentValue::Single(value) => vec![value],
                    RawArgumentValue::Many(values) => values,
                },
            },
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct AssetIndex {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub sha1: String,
    #[serde(default)]
    pub size: i64,
    #[serde(rename = "totalSize", default)]
    pub total_size: i64,
    #[serde(default)]
    pub url: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Downloads {
    #[serde(default)]
    pub client: Option<DownloadEntry>,
    #[serde(default)]
    pub server: Option<DownloadEntry>,
    #[serde(rename = "client_mappings", default)]
    pub client_mappings: Option<DownloadEntry>,
    #[serde(rename = "server_mappings", default)]
    pub server_mappings: Option<DownloadEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct DownloadEntry {
    #[serde(default)]
    pub sha1: String,
    #[serde(default)]
    pub size: i64,
    #[serde(default)]
    pub url: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct JavaVersion {
    #[serde(default)]
    pub component: String,
    #[serde(rename = "majorVersion", default)]
    pub major_version: i32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Library {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub downloads: Option<LibraryDownload>,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub rules: Vec<Rule>,
    #[serde(default)]
    pub natives: HashMap<String, String>,
    #[serde(default)]
    pub extract: Option<ExtractRule>,
    #[serde(default)]
    pub sha1: String,
    #[serde(default)]
    pub sha256: String,
    #[serde(default)]
    pub size: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct LibraryDownload {
    #[serde(default)]
    pub artifact: Option<LibraryArtifact>,
    #[serde(default)]
    pub classifiers: HashMap<String, LibraryArtifact>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct LibraryArtifact {
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub sha1: String,
    #[serde(default)]
    pub size: i64,
    #[serde(default)]
    pub url: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ExtractRule {
    #[serde(default)]
    pub exclude: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct LoggingConf {
    #[serde(default)]
    pub client: Option<LoggingEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct LoggingEntry {
    #[serde(default)]
    pub argument: String,
    pub file: LoggingFile,
    #[serde(default)]
    pub kind: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct LoggingFile {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub sha1: String,
    #[serde(default)]
    pub size: i64,
    #[serde(default)]
    pub url: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedLibrary {
    pub abs_path: PathBuf,
    pub is_native: bool,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchVars {
    pub auth_player_name: String,
    pub version_name: String,
    pub game_directory: String,
    pub assets_root: String,
    pub asset_index_name: String,
    pub auth_uuid: String,
    pub auth_access_token: String,
    pub client_id: String,
    pub auth_xuid: String,
    pub user_type: String,
    pub version_type: String,
    pub launcher_name: String,
    pub launcher_version: String,
    pub natives_directory: String,
    pub classpath: String,
    pub library_directory: String,
    pub classpath_separator: String,
    pub resolution_width: String,
    pub resolution_height: String,
    pub game_assets: String,
}

#[derive(Debug, Error)]
pub enum LaunchModelError {
    #[error("reading version {version_id}: {source}")]
    ReadVersion {
        version_id: String,
        #[source]
        source: std::io::Error,
    },
    #[error("parsing version {version_id}: {source}")]
    ParseVersion {
        version_id: String,
        #[source]
        source: serde_json::Error,
    },
    #[error("loading parent {parent_id} for {child_id}: {source}")]
    LoadParent {
        child_id: String,
        parent_id: String,
        #[source]
        source: Box<LaunchModelError>,
    },
    #[error("inheritsFrom chain too deep (>10) for {version_id}")]
    InheritanceTooDeep { version_id: String },
}

pub fn load_version_json(mc_dir: &Path, version_id: &str) -> Result<VersionJson, LaunchModelError> {
    let path = versions_dir(mc_dir)
        .join(version_id)
        .join(format!("{version_id}.json"));
    let data = fs::read_to_string(&path).map_err(|source| LaunchModelError::ReadVersion {
        version_id: version_id.to_string(),
        source,
    })?;
    let mut version: VersionJson =
        serde_json::from_str(&data).map_err(|source| LaunchModelError::ParseVersion {
            version_id: version_id.to_string(),
            source,
        })?;

    if version.asset_index.id.is_empty() && !version.assets.is_empty() {
        version.asset_index.id = version.assets.clone();
    }

    Ok(version)
}

pub fn resolve_version(mc_dir: &Path, version_id: &str) -> Result<VersionJson, LaunchModelError> {
    let version = load_version_json(mc_dir, version_id)?;
    if version.inherits_from.is_empty() {
        return Ok(version);
    }
    resolve_inheritance(mc_dir, version, 0)
}

pub fn resolve_libraries(
    version: &VersionJson,
    mc_dir: &Path,
    env: &Environment,
) -> Vec<ResolvedLibrary> {
    let lib_dir = libraries_dir(mc_dir);
    let mut resolved = Vec::new();
    let mut seen_libraries = HashSet::new();

    for lib in &version.libraries {
        if !seen_libraries.insert(library_merge_key(&lib.name)) {
            continue;
        }

        if !evaluate_rules(&lib.rules, env) {
            continue;
        }

        let is_native = is_native_library(&lib.name);
        if is_native && !native_name_matches_env(&lib.name, env) {
            continue;
        }

        if !lib.natives.is_empty() {
            resolved.extend(resolve_legacy_natives(lib, &lib_dir, env));
            if lib
                .downloads
                .as_ref()
                .is_some_and(|downloads| downloads.artifact.is_none())
            {
                continue;
            }
        }

        if let Some(artifact) = lib
            .downloads
            .as_ref()
            .and_then(|downloads| downloads.artifact.as_ref())
        {
            resolved.push(ResolvedLibrary {
                abs_path: lib_dir.join(PathBuf::from(&artifact.path)),
                is_native,
                name: lib.name.clone(),
            });
            continue;
        }

        let maven_path = maven_to_path(&lib.name);
        if maven_path.as_os_str().is_empty() {
            continue;
        }

        resolved.push(ResolvedLibrary {
            abs_path: lib_dir.join(maven_path),
            is_native,
            name: lib.name.clone(),
        });
    }

    resolved
}

pub fn build_classpath(libraries: &[ResolvedLibrary], client_jar_path: Option<&Path>) -> String {
    let mut seen = HashMap::<String, ()>::new();
    let mut parts = Vec::new();

    for lib in libraries {
        let key = lib.abs_path.to_string_lossy().to_string();
        if seen.contains_key(&key) {
            continue;
        }
        seen.insert(key, ());
        parts.push(lib.abs_path.to_string_lossy().to_string());
    }

    if let Some(client_jar_path) = client_jar_path {
        let key = client_jar_path.to_string_lossy().to_string();
        if !seen.contains_key(&key) {
            parts.push(key);
        }
    }

    parts.join(if cfg!(target_os = "windows") {
        ";"
    } else {
        ":"
    })
}

pub fn resolve_arguments(
    version: &VersionJson,
    env: &Environment,
    vars: &LaunchVars,
) -> (Vec<String>, Vec<String>) {
    let var_map = vars.build_var_map();
    let (mut jvm_args, mut game_args) = if version.is_legacy_version() {
        (
            default_legacy_jvm_args(&var_map),
            resolve_legacy_args(&version.minecraft_arguments, &var_map),
        )
    } else if let Some(arguments) = &version.arguments {
        (
            resolve_arg_list(&arguments.jvm, env, &var_map),
            resolve_arg_list(&arguments.game, env, &var_map),
        )
    } else {
        (Vec::new(), Vec::new())
    };

    if let Some(client_logging) = version
        .logging
        .as_ref()
        .and_then(|logging| logging.client.as_ref())
    {
        let log_arg = resolve_logging_arg(client_logging, &vars.assets_root);
        if !log_arg.is_empty() {
            jvm_args.push(log_arg);
        }
    }

    game_args.retain(|arg| arg != "--demo");
    (jvm_args, game_args)
}

pub fn offline_uuid(username: &str) -> String {
    let data = format!("OfflinePlayer:{username}");
    let digest = md5::compute(data.as_bytes());
    let mut hash = digest.0;
    hash[6] = (hash[6] & 0x0f) | 0x30;
    hash[8] = (hash[8] & 0x3f) | 0x80;

    format!(
        "{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        hash[0],
        hash[1],
        hash[2],
        hash[3],
        hash[4],
        hash[5],
        hash[6],
        hash[7],
        hash[8],
        hash[9],
        hash[10],
        hash[11],
        hash[12],
        hash[13],
        hash[14],
        hash[15]
    )
}

impl LaunchVars {
    pub fn build_var_map(&self) -> HashMap<&str, String> {
        let mut vars = HashMap::new();
        vars.insert("auth_player_name", self.auth_player_name.clone());
        vars.insert("version_name", self.version_name.clone());
        vars.insert("game_directory", self.game_directory.clone());
        vars.insert("assets_root", self.assets_root.clone());
        vars.insert("assets_index_name", self.asset_index_name.clone());
        vars.insert("auth_uuid", self.auth_uuid.clone());
        vars.insert("auth_access_token", self.auth_access_token.clone());
        vars.insert("clientid", self.client_id.clone());
        vars.insert("auth_xuid", self.auth_xuid.clone());
        vars.insert("user_type", self.user_type.clone());
        vars.insert("version_type", self.version_type.clone());
        vars.insert("launcher_name", self.launcher_name.clone());
        vars.insert("launcher_version", self.launcher_version.clone());
        vars.insert("natives_directory", self.natives_directory.clone());
        vars.insert("classpath", self.classpath.clone());
        vars.insert("library_directory", self.library_directory.clone());
        vars.insert("classpath_separator", self.classpath_separator.clone());
        vars.insert("resolution_width", self.resolution_width.clone());
        vars.insert("resolution_height", self.resolution_height.clone());
        vars.insert("game_assets", self.game_assets_dir());
        vars.insert("user_properties", "{}".to_string());
        vars
    }

    fn game_assets_dir(&self) -> String {
        if self.game_assets.is_empty() {
            self.assets_root.clone()
        } else {
            self.game_assets.clone()
        }
    }
}

pub fn client_jar_path(
    mc_dir: &Path,
    version: &VersionJson,
    requested_version_id: &str,
) -> PathBuf {
    let version_id = if version.id.is_empty() {
        requested_version_id
    } else {
        version.id.as_str()
    };
    versions_dir(mc_dir)
        .join(version_id)
        .join(format!("{version_id}.jar"))
}

fn resolve_inheritance(
    mc_dir: &Path,
    child: VersionJson,
    depth: usize,
) -> Result<VersionJson, LaunchModelError> {
    if depth > 10 {
        return Err(LaunchModelError::InheritanceTooDeep {
            version_id: child.id.clone(),
        });
    }

    if child.inherits_from.is_empty() {
        return Ok(child);
    }

    let parent_id = child.inherits_from.clone();
    let mut parent =
        load_version_json(mc_dir, &parent_id).map_err(|source| LaunchModelError::LoadParent {
            child_id: child.id.clone(),
            parent_id: parent_id.clone(),
            source: Box::new(source),
        })?;
    if !parent.inherits_from.is_empty() {
        parent = resolve_inheritance(mc_dir, parent, depth + 1)?;
    }

    Ok(merge_versions(&parent, &child))
}

fn merge_versions(parent: &VersionJson, child: &VersionJson) -> VersionJson {
    let mut merged = VersionJson {
        id: child.id.clone(),
        inherits_from: String::new(),
        kind: non_empty(&child.kind, &parent.kind),
        main_class: non_empty(&child.main_class, &parent.main_class),
        minimum_launcher_version: if child.minimum_launcher_version != 0 {
            child.minimum_launcher_version
        } else {
            parent.minimum_launcher_version
        },
        compliance_level: if child.compliance_level != 0 {
            child.compliance_level
        } else {
            parent.compliance_level
        },
        release_time: non_empty(&child.release_time, &parent.release_time),
        time: non_empty(&child.time, &parent.time),
        arguments: None,
        minecraft_arguments: if !child.minecraft_arguments.is_empty() {
            child.minecraft_arguments.clone()
        } else {
            parent.minecraft_arguments.clone()
        },
        asset_index: if !child.asset_index.id.is_empty() {
            child.asset_index.clone()
        } else {
            parent.asset_index.clone()
        },
        assets: non_empty(&child.assets, &parent.assets),
        downloads: parent.downloads.clone(),
        java_version: merge_java_version(&parent.java_version, &child.java_version),
        libraries: Vec::new(),
        logging: child.logging.clone().or_else(|| parent.logging.clone()),
    };

    merged.libraries = merge_libraries_prefer_first(&child.libraries, &parent.libraries);

    if parent.arguments.is_some() || child.arguments.is_some() {
        let mut arguments = ArgumentsSection::default();
        if let Some(parent_args) = &parent.arguments {
            arguments.game.extend(parent_args.game.clone());
            arguments.jvm.extend(parent_args.jvm.clone());
        }
        if let Some(child_args) = &child.arguments {
            arguments.game.extend(child_args.game.clone());
            arguments.jvm.extend(child_args.jvm.clone());
        }
        merged.arguments = Some(arguments);
    }

    merged
}

pub fn merge_libraries_prefer_first(preferred: &[Library], fallback: &[Library]) -> Vec<Library> {
    let mut seen = HashSet::new();
    let mut merged = Vec::with_capacity(preferred.len() + fallback.len());

    for library in preferred.iter().chain(fallback.iter()) {
        let key = library_merge_key(&library.name);
        if !seen.insert(key) {
            continue;
        }
        merged.push(library.clone());
    }

    merged
}

pub(crate) fn library_merge_key(name: &str) -> String {
    let parts = name.split(':').collect::<Vec<_>>();
    if parts.len() < 2 {
        return name.to_string();
    }

    let mut key = format!("{}:{}", parts[0], parts[1]);
    if let Some(classifier) = parts.get(3) {
        key.push(':');
        key.push_str(classifier);
    }
    key
}

fn merge_java_version(parent: &JavaVersion, child: &JavaVersion) -> JavaVersion {
    JavaVersion {
        component: if child.component.is_empty() {
            parent.component.clone()
        } else {
            child.component.clone()
        },
        major_version: if child.major_version == 0 {
            parent.major_version
        } else {
            child.major_version
        },
    }
}

fn resolve_legacy_natives(
    lib: &Library,
    lib_dir: &Path,
    env: &Environment,
) -> Vec<ResolvedLibrary> {
    let Some(base_classifier) = lib.natives.get(&env.os_name) else {
        return Vec::new();
    };

    for classifier_key in native_classifier_candidates(base_classifier, &env.os_arch) {
        if let Some(artifact) = lib
            .downloads
            .as_ref()
            .and_then(|downloads| downloads.classifiers.get(&classifier_key))
        {
            return vec![ResolvedLibrary {
                abs_path: lib_dir.join(PathBuf::from(&artifact.path)),
                is_native: true,
                name: format!("{}:{classifier_key}", lib.name),
            }];
        }
    }

    let Some(classifier_key) = native_classifier_candidates(base_classifier, &env.os_arch)
        .into_iter()
        .next()
    else {
        return Vec::new();
    };
    let maven_path = maven_to_path(&format!("{}:{classifier_key}", lib.name));
    if maven_path.as_os_str().is_empty() {
        return Vec::new();
    }

    vec![ResolvedLibrary {
        abs_path: lib_dir.join(maven_path),
        is_native: true,
        name: format!("{}:{classifier_key}", lib.name),
    }]
}

fn native_classifier_candidates(base_classifier: &str, os_arch: &str) -> Vec<String> {
    let mut candidates = Vec::new();
    let variants = match os_arch {
        "x86_64" => vec![
            base_classifier.replace("${arch}", "64"),
            base_classifier.replace("-${arch}", ""),
            base_classifier.replace("${arch}", "x86_64"),
        ],
        "x86" => vec![
            base_classifier.replace("${arch}", "32"),
            base_classifier.replace("${arch}", "x86"),
        ],
        "arm64" => vec![
            base_classifier.replace("${arch}", "arm64"),
            base_classifier.replace("${arch}", "64"),
        ],
        _ => vec![base_classifier.replace("${arch}", os_arch)],
    };

    for variant in variants {
        if !variant.is_empty() && !candidates.contains(&variant) {
            candidates.push(variant);
        }
    }

    candidates
}

fn native_name_matches_env(name: &str, env: &Environment) -> bool {
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

fn resolve_arg_list(
    args: &[Argument],
    env: &Environment,
    var_map: &HashMap<&str, String>,
) -> Vec<String> {
    let mut resolved = Vec::new();
    for arg in args {
        if !evaluate_rules(&arg.rules, env) {
            continue;
        }
        for value in &arg.value {
            resolved.push(substitute_vars(value, var_map));
        }
    }
    resolved
}

fn resolve_legacy_args(arguments: &str, var_map: &HashMap<&str, String>) -> Vec<String> {
    arguments
        .split_whitespace()
        .map(|part| substitute_vars(part, var_map))
        .collect()
}

fn default_legacy_jvm_args(var_map: &HashMap<&str, String>) -> Vec<String> {
    vec![
        format!(
            "-Djava.library.path={}",
            var_map
                .get("natives_directory")
                .cloned()
                .unwrap_or_default()
        ),
        format!(
            "-Dminecraft.launcher.brand={}",
            var_map.get("launcher_name").cloned().unwrap_or_default()
        ),
        format!(
            "-Dminecraft.launcher.version={}",
            var_map.get("launcher_version").cloned().unwrap_or_default()
        ),
        "-cp".to_string(),
        var_map.get("classpath").cloned().unwrap_or_default(),
    ]
}

fn resolve_logging_arg(entry: &LoggingEntry, assets_root: &str) -> String {
    if entry.argument.is_empty() || entry.file.id.is_empty() {
        return String::new();
    }

    entry.argument.replace(
        "${path}",
        &Path::new(assets_root)
            .join("log_configs")
            .join(&entry.file.id)
            .to_string_lossy(),
    )
}

fn substitute_vars(input: &str, var_map: &HashMap<&str, String>) -> String {
    let mut output = input.to_string();
    for (key, value) in var_map {
        output = output.replace(&format!("${{{key}}}"), value);
    }
    output
}

pub fn maven_to_path(coordinate: &str) -> PathBuf {
    let mut coordinate = coordinate.to_string();
    let mut extension = "jar".to_string();
    if let Some(index) = coordinate.rfind('@') {
        let raw_extension = coordinate[(index + 1)..].trim().trim_start_matches('.');
        if !raw_extension.is_empty() {
            extension = raw_extension.to_string();
        }
        coordinate.truncate(index);
    }

    let parts = coordinate.split(':').collect::<Vec<_>>();
    if parts.len() < 3 {
        return PathBuf::new();
    }

    let group = parts[0].replace('.', std::path::MAIN_SEPARATOR_STR);
    let artifact = parts[1];
    let version = parts[2];

    let mut filename = format!("{artifact}-{version}");
    if let Some(classifier) = parts.get(3) {
        filename.push('-');
        filename.push_str(classifier);
    }
    filename.push('.');
    filename.push_str(&extension);

    PathBuf::from(group)
        .join(artifact)
        .join(version)
        .join(filename)
}

fn non_empty(preferred: &str, fallback: &str) -> String {
    if preferred.is_empty() {
        fallback.to_string()
    } else {
        preferred.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rules::default_environment;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn resolve_arguments_strips_demo_from_legacy_versions() {
        let version = VersionJson {
            id: "1.5.2".to_string(),
            minecraft_arguments: "--username ${auth_player_name} --demo".to_string(),
            ..default_version()
        };
        let vars = default_launch_vars();
        let (_jvm_args, game_args) = resolve_arguments(&version, &default_environment(), &vars);

        assert_eq!(
            game_args,
            vec!["--username".to_string(), "Player".to_string()]
        );
    }

    #[test]
    fn resolve_version_inherits_java_and_arguments() {
        let temp_root = std::env::temp_dir().join(format!(
            "croopor-launch-test-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("time")
                .as_nanos()
        ));
        let versions_dir = temp_root.join("versions");
        fs::create_dir_all(versions_dir.join("base")).expect("base dir");
        fs::create_dir_all(versions_dir.join("child")).expect("child dir");

        fs::write(
            versions_dir.join("base").join("base.json"),
            serde_json::json!({
                "id": "base",
                "type": "release",
                "mainClass": "net.minecraft.client.main.Main",
                "arguments": {
                    "game": ["--base"],
                    "jvm": ["-Dbase=true"]
                },
                "assetIndex": { "id": "1.20.1" },
                "javaVersion": { "component": "java-runtime-gamma", "majorVersion": 17 },
                "libraries": []
            })
            .to_string(),
        )
        .expect("write base");
        fs::write(
            versions_dir.join("child").join("child.json"),
            serde_json::json!({
                "id": "child",
                "inheritsFrom": "base",
                "type": "release",
                "arguments": {
                    "game": ["--child"],
                    "jvm": ["-Dchild=true"]
                },
                "assetIndex": { "id": "1.20.1" },
                "libraries": []
            })
            .to_string(),
        )
        .expect("write child");

        let resolved = resolve_version(&temp_root, "child").expect("resolve inherited version");
        assert_eq!(resolved.java_version.major_version, 17);
        let arguments = resolved.arguments.expect("merged arguments");
        assert_eq!(arguments.jvm.len(), 2);
        assert_eq!(arguments.game.len(), 2);

        let _ = fs::remove_dir_all(&temp_root);
    }

    fn default_version() -> VersionJson {
        VersionJson {
            id: "test".to_string(),
            inherits_from: String::new(),
            kind: "release".to_string(),
            main_class: "net.minecraft.client.main.Main".to_string(),
            minimum_launcher_version: 0,
            compliance_level: 0,
            release_time: String::new(),
            time: String::new(),
            arguments: None,
            minecraft_arguments: String::new(),
            asset_index: AssetIndex {
                id: "1.20.1".to_string(),
                ..AssetIndex::default()
            },
            assets: String::new(),
            downloads: Downloads::default(),
            java_version: JavaVersion::default(),
            libraries: Vec::new(),
            logging: None,
        }
    }

    fn default_launch_vars() -> LaunchVars {
        LaunchVars {
            auth_player_name: "Player".to_string(),
            version_name: "test".to_string(),
            game_directory: ".".to_string(),
            assets_root: ".".to_string(),
            asset_index_name: "1.20.1".to_string(),
            auth_uuid: offline_uuid("Player"),
            auth_access_token: "null".to_string(),
            client_id: String::new(),
            auth_xuid: String::new(),
            user_type: "legacy".to_string(),
            version_type: "release".to_string(),
            launcher_name: "croopor".to_string(),
            launcher_version: "1.0.0".to_string(),
            natives_directory: String::new(),
            classpath: "client.jar".to_string(),
            library_directory: "libraries".to_string(),
            classpath_separator: if cfg!(target_os = "windows") {
                ";".to_string()
            } else {
                ":".to_string()
            },
            resolution_width: String::new(),
            resolution_height: String::new(),
            game_assets: String::new(),
        }
    }

    #[test]
    fn logging_arg_uses_assets_root_once() {
        let entry = LoggingEntry {
            argument: "-Dlog4j.configurationFile=${path}".to_string(),
            file: LoggingFile {
                id: "client-1.12.xml".to_string(),
                ..LoggingFile::default()
            },
            ..LoggingEntry::default()
        };

        let resolved = resolve_logging_arg(&entry, "C:/croopor/library/assets");
        assert_eq!(
            resolved,
            "-Dlog4j.configurationFile=C:/croopor/library/assets/log_configs/client-1.12.xml"
        );
    }

    #[test]
    fn native_classifier_prefers_windows_x64_fallback() {
        let candidates = native_classifier_candidates("natives-windows-${arch}", "x86_64");
        assert_eq!(candidates[0], "natives-windows-64");
        assert!(
            candidates
                .iter()
                .any(|candidate| candidate == "natives-windows")
        );
    }

    #[test]
    fn merge_libraries_prefers_child_version_for_same_artifact() {
        let merged = merge_libraries_prefer_first(
            &[Library {
                name: "org.ow2.asm:asm:9.9".to_string(),
                ..Library::default()
            }],
            &[Library {
                name: "org.ow2.asm:asm:9.6".to_string(),
                ..Library::default()
            }],
        );

        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].name, "org.ow2.asm:asm:9.9");
    }

    #[test]
    fn merge_libraries_keeps_distinct_classifiers() {
        let merged = merge_libraries_prefer_first(
            &[
                Library {
                    name: "org.lwjgl:lwjgl:3.3.3".to_string(),
                    ..Library::default()
                },
                Library {
                    name: "org.lwjgl:lwjgl:3.3.3:natives-windows".to_string(),
                    ..Library::default()
                },
            ],
            &[],
        );

        assert_eq!(merged.len(), 2);
    }

    #[test]
    fn resolve_libraries_skips_duplicate_artifact_versions_from_existing_manifest() {
        let mut version = default_version();
        version.libraries = vec![
            Library {
                name: "org.ow2.asm:asm:9.9".to_string(),
                ..Library::default()
            },
            Library {
                name: "org.ow2.asm:asm:9.6".to_string(),
                ..Library::default()
            },
        ];

        let env = default_environment();
        let resolved = resolve_libraries(&version, Path::new("/tmp/croopor"), &env);
        assert_eq!(resolved.len(), 1);
        assert!(
            resolved[0]
                .abs_path
                .to_string_lossy()
                .contains("org/ow2/asm/asm/9.9/asm-9.9.jar")
        );
    }
}
