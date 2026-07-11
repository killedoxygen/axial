pub mod steps;

use crate::runtime::RuntimeSelection;
use axial_minecraft::{
    LaunchModelError, LaunchVars, ResolvedLibrary, VersionJson,
    asset_index_requires_virtual_repair, build_classpath, client_jar_path, default_environment,
    libraries_dir, offline_uuid, resolve_arguments, resolve_libraries, resolve_version,
};
use md5::compute as md5_compute;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use thiserror::Error;
use zip::ZipArchive;

#[derive(Clone, PartialEq, Eq)]
pub struct LaunchAuthContext {
    pub player_name: String,
    pub uuid: String,
    pub access_token: String,
    pub client_id: String,
    pub xuid: String,
    pub user_type: String,
}

const OFFLINE_AUTH_ACCESS_TOKEN: &str = "0";
const OFFLINE_AUTH_USER_TYPE: &str = "msa";
const AUTHLIB_OFFLINE_MULTIPLAYER_VERSIONS: &[&str] = &["1.16.4", "1.16.5"];
const AUTHLIB_OFFLINE_MULTIPLAYER_JVM_ARGS: &[&str] = &[
    "-Dminecraft.api.env=custom",
    "-Dminecraft.api.auth.host=https://nope.invalid",
    "-Dminecraft.api.account.host=https://nope.invalid",
    "-Dminecraft.api.session.host=https://nope.invalid",
    "-Dminecraft.api.services.host=https://nope.invalid",
];
const LEGACY_FML_LIBRARY_MIRROR_JVM_ARG: &str = "-Dfml.core.libraries.mirror=https://web.archive.org/web/20200830040255if_/http://files.minecraftforge.net/fmllibs/%s";

impl LaunchAuthContext {
    pub fn offline(player_name: impl Into<String>) -> Self {
        let player_name = player_name.into();
        Self {
            uuid: offline_uuid(&player_name),
            player_name,
            access_token: OFFLINE_AUTH_ACCESS_TOKEN.to_string(),
            client_id: String::new(),
            xuid: String::new(),
            user_type: OFFLINE_AUTH_USER_TYPE.to_string(),
        }
    }

    pub fn is_offline(&self) -> bool {
        self.access_token == OFFLINE_AUTH_ACCESS_TOKEN
            && self.client_id.is_empty()
            && self.xuid.is_empty()
            && self.uuid == offline_uuid(&self.player_name)
    }
}

impl fmt::Debug for LaunchAuthContext {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LaunchAuthContext")
            .field("player_name", &self.player_name)
            .field("uuid", &self.uuid)
            .field("access_token", &"[redacted]")
            .field("client_id", &self.client_id)
            .field("xuid", &self.xuid)
            .field("user_type", &self.user_type)
            .finish()
    }
}

#[derive(Debug, Clone)]
pub struct VanillaLaunchRequest {
    pub session_id: String,
    pub mc_dir: PathBuf,
    pub version_id: String,
    pub target_version_id: String,
    pub auth: LaunchAuthContext,
    pub runtime: RuntimeSelection,
    pub game_dir: Option<PathBuf>,
    pub launcher_name: String,
    pub launcher_version: String,
    pub min_memory_mb: Option<i32>,
    pub max_memory_mb: Option<i32>,
    pub extra_jvm_args: Vec<String>,
    pub resolution: Option<(u32, u32)>,
}

#[derive(Debug, Clone)]
pub struct VanillaLaunchPlan {
    pub version: VersionJson,
    pub requires_virtual_asset_repair: bool,
    pub libraries: Vec<ResolvedLibrary>,
    pub client_jar_path: Option<PathBuf>,
    pub natives_dir: Option<PathBuf>,
    pub classpath: String,
    pub jvm_args: Vec<String>,
    pub game_args: Vec<String>,
    pub main_class: String,
    pub command: Vec<String>,
    pub game_dir: PathBuf,
}

#[derive(Debug, Error)]
pub enum VanillaLaunchPlanError {
    #[error(transparent)]
    LaunchModel(#[from] LaunchModelError),
    #[error(transparent)]
    AssetIndexFlags(#[from] axial_minecraft::AssetIndexFlagsError),
    #[error("effective runtime path is empty")]
    MissingRuntime,
    #[error("failed to prepare legacy natives: {0}")]
    PrepareNatives(#[from] io::Error),
    #[error("failed to extract legacy natives: {0}")]
    ExtractNatives(#[from] zip::result::ZipError),
}

pub fn plan_vanilla_launch(
    request: &VanillaLaunchRequest,
) -> Result<VanillaLaunchPlan, VanillaLaunchPlanError> {
    if request.runtime.effective_path.trim().is_empty() {
        return Err(VanillaLaunchPlanError::MissingRuntime);
    }

    let version = resolve_version(&request.mc_dir, &request.version_id)?;
    plan_resolved_launch(request, version)
}

pub fn plan_resolved_launch(
    request: &VanillaLaunchRequest,
    version: VersionJson,
) -> Result<VanillaLaunchPlan, VanillaLaunchPlanError> {
    let client_jar = find_client_jar(&request.mc_dir, &version, &request.version_id);
    let requires_virtual_asset_repair =
        asset_index_requires_virtual_repair(&request.mc_dir, &version.asset_index.id)?;

    let mut env = default_environment();
    let game_dir = request
        .game_dir
        .clone()
        .unwrap_or_else(|| request.mc_dir.clone());
    if let Some((width, height)) = request.resolution {
        env.features
            .insert("has_custom_resolution".to_string(), true);
        let _ = (width, height);
    }

    let libraries = resolve_libraries(&version, &request.mc_dir, &env);
    let classpath = build_classpath(&libraries, client_jar.as_deref());
    let needs_natives_dir = libraries.iter().any(|library| library.is_native);
    let natives_dir = if needs_natives_dir {
        Some(create_natives_dir(&request.version_id, &libraries)?)
    } else {
        None
    };
    let game_assets = if requires_virtual_asset_repair {
        request
            .mc_dir
            .join("assets")
            .join("virtual")
            .join("legacy")
            .to_string_lossy()
            .to_string()
    } else {
        String::new()
    };

    let (resolution_width, resolution_height) = request
        .resolution
        .map(|(width, height)| (width.to_string(), height.to_string()))
        .unwrap_or_default();

    let vars = LaunchVars {
        auth_player_name: request.auth.player_name.clone(),
        version_name: version.id.clone(),
        game_directory: game_dir.to_string_lossy().to_string(),
        assets_root: request.mc_dir.join("assets").to_string_lossy().to_string(),
        asset_index_name: version.asset_index.id.clone(),
        auth_uuid: request.auth.uuid.clone(),
        auth_access_token: request.auth.access_token.clone(),
        client_id: request.auth.client_id.clone(),
        auth_xuid: request.auth.xuid.clone(),
        user_type: request.auth.user_type.clone(),
        version_type: version.kind.clone(),
        launcher_name: request.launcher_name.clone(),
        launcher_version: request.launcher_version.clone(),
        natives_directory: natives_dir
            .as_ref()
            .map(|path| path.to_string_lossy().to_string())
            .unwrap_or_default(),
        classpath: classpath.clone(),
        library_directory: libraries_dir(&request.mc_dir).to_string_lossy().to_string(),
        classpath_separator: if cfg!(target_os = "windows") {
            ";".to_string()
        } else {
            ":".to_string()
        },
        resolution_width,
        resolution_height,
        game_assets,
    };
    let (mut jvm_args, game_args) = resolve_arguments(&version, &env, &vars);

    if let Some(max_memory_mb) = request.max_memory_mb.filter(|value| *value > 0) {
        jvm_args.push(format!("-Xmx{max_memory_mb}M"));
    }
    if let Some(min_memory_mb) = request.min_memory_mb.filter(|value| *value > 0) {
        jvm_args.push(format!("-Xms{min_memory_mb}M"));
    }
    if let Some(natives_dir) = natives_dir.as_ref() {
        let natives_path = natives_dir.to_string_lossy().to_string();
        jvm_args.push(format!("-Djava.library.path={natives_path}"));
        jvm_args.push(format!("-Dorg.lwjgl.librarypath={natives_path}"));
        jvm_args.push(format!(
            "-Dorg.lwjgl.system.SharedLibraryExtractPath={natives_path}"
        ));
        jvm_args.push(format!("-Djna.tmpdir={natives_path}"));
        jvm_args.push(format!("-Djava.io.tmpdir={natives_path}"));
    }
    jvm_args.extend(authlib_offline_multiplayer_jvm_args(request, &version));
    jvm_args.extend(legacy_fml_library_mirror_jvm_args(request, &version));
    jvm_args.extend(request.extra_jvm_args.clone());
    let main_class = version.main_class.clone();

    let mut command = Vec::with_capacity(2 + jvm_args.len() + game_args.len());
    command.push(request.runtime.effective_path.clone());
    command.extend(jvm_args.clone());
    command.push(main_class.clone());
    command.extend(game_args.clone());

    Ok(VanillaLaunchPlan {
        version,
        requires_virtual_asset_repair,
        libraries,
        client_jar_path: client_jar,
        natives_dir,
        classpath,
        jvm_args,
        game_args,
        main_class,
        command,
        game_dir,
    })
}

fn authlib_offline_multiplayer_jvm_args(
    request: &VanillaLaunchRequest,
    version: &VersionJson,
) -> Vec<String> {
    if !request.auth.is_offline() {
        return Vec::new();
    }
    let target_version = effective_minecraft_version_id(request, version);
    if AUTHLIB_OFFLINE_MULTIPLAYER_VERSIONS.contains(&target_version) {
        return AUTHLIB_OFFLINE_MULTIPLAYER_JVM_ARGS
            .iter()
            .map(|arg| (*arg).to_string())
            .collect();
    }
    Vec::new()
}

fn legacy_fml_library_mirror_jvm_args(
    request: &VanillaLaunchRequest,
    version: &VersionJson,
) -> Vec<String> {
    let target_version = effective_minecraft_version_id(request, version);
    if !matches!(target_version, "1.5" | "1.5.1" | "1.5.2") {
        return Vec::new();
    }
    if !version
        .libraries
        .iter()
        .any(|library| library.name == "net.minecraftforge:legacyfixer:1.0")
    {
        return Vec::new();
    }
    vec![LEGACY_FML_LIBRARY_MIRROR_JVM_ARG.to_string()]
}

fn effective_minecraft_version_id<'a>(
    request: &'a VanillaLaunchRequest,
    version: &'a VersionJson,
) -> &'a str {
    let target = request.target_version_id.trim();
    if !target.is_empty() {
        return target;
    }
    let inherited = version.inherits_from.trim();
    if !inherited.is_empty() {
        return inherited;
    }
    let version_id = version.id.trim();
    if !version_id.is_empty() {
        return version_id;
    }
    request.version_id.trim()
}

pub fn cleanup_natives_dir(dir: &Path) -> io::Result<()> {
    let cleaned = dir.to_string_lossy();
    let managed_legacy = format!(
        "{}axial{}natives",
        std::path::MAIN_SEPARATOR,
        std::path::MAIN_SEPARATOR
    );
    let managed_cache = format!(
        "{}axial{}cache{}natives",
        std::path::MAIN_SEPARATOR,
        std::path::MAIN_SEPARATOR,
        std::path::MAIN_SEPARATOR
    );
    if !cleaned.contains(&managed_legacy)
        && !cleaned.ends_with(&format!("axial{}natives", std::path::MAIN_SEPARATOR))
        && !cleaned.contains(&managed_cache)
        && !cleaned.ends_with(&format!(
            "axial{}cache{}natives",
            std::path::MAIN_SEPARATOR,
            std::path::MAIN_SEPARATOR
        ))
    {
        return Err(io::Error::other(format!(
            "refusing to remove non-axial natives directory: {}",
            dir.display()
        )));
    }
    fs::remove_dir_all(dir)
}

pub(crate) fn find_client_jar(
    mc_dir: &Path,
    version: &VersionJson,
    original_version_id: &str,
) -> Option<PathBuf> {
    let versions_dir = mc_dir.join("versions");
    let child_manifest = read_child_manifest_summary(&versions_dir, original_version_id);

    let primary = client_jar_path(mc_dir, version, original_version_id);
    if primary.is_file() {
        return Some(primary);
    }

    if !original_version_id.is_empty()
        && let Some(child_jar) = first_jar_in_version_dir(&versions_dir.join(original_version_id))
    {
        return Some(child_jar);
    }

    if child_manifest
        .as_ref()
        .is_some_and(|summary| summary.has_client_artifact)
    {
        return None;
    }

    if let Some(parent_id) = child_manifest
        .as_ref()
        .and_then(|summary| summary.parent_id.as_deref())
    {
        let parent_jar = versions_dir
            .join(parent_id)
            .join(format!("{parent_id}.jar"));
        if parent_jar.is_file() {
            return Some(parent_jar);
        }
    }

    if version.id.is_empty() {
        None
    } else {
        first_jar_in_version_dir(&versions_dir.join(&version.id))
    }
}

struct ChildManifestSummary {
    parent_id: Option<String>,
    has_client_artifact: bool,
}

fn read_child_manifest_summary(
    versions_dir: &Path,
    original_version_id: &str,
) -> Option<ChildManifestSummary> {
    if original_version_id.is_empty() {
        return None;
    }

    #[derive(serde::Deserialize)]
    struct StubVersion {
        #[serde(rename = "inheritsFrom", default)]
        inherits_from: String,
        #[serde(default)]
        downloads: StubDownloads,
    }

    #[derive(Default, serde::Deserialize)]
    struct StubDownloads {
        #[serde(default)]
        client: Option<serde_json::Value>,
    }

    let original_json_path = versions_dir
        .join(original_version_id)
        .join(format!("{original_version_id}.json"));
    let data = fs::read_to_string(original_json_path).ok()?;
    let stub = serde_json::from_str::<StubVersion>(&data).ok()?;
    Some(ChildManifestSummary {
        parent_id: (!stub.inherits_from.trim().is_empty()).then_some(stub.inherits_from),
        has_client_artifact: stub.downloads.client.is_some(),
    })
}

fn first_jar_in_version_dir(version_dir: &Path) -> Option<PathBuf> {
    let entries = fs::read_dir(version_dir).ok()?;
    for entry in entries.flatten() {
        if entry.path().extension().is_some_and(|ext| ext == "jar") {
            return Some(entry.path());
        }
    }
    None
}

fn create_natives_dir(version_id: &str, libraries: &[ResolvedLibrary]) -> io::Result<PathBuf> {
    let root = natives_cache_root()?;
    fs::create_dir_all(&root)?;

    let cache_key = native_cache_key(version_id, libraries);
    let ready_dir = root.join(&cache_key);
    let ready_marker = ready_dir.join(".ready");
    if ready_marker.is_file() {
        return Ok(ready_dir);
    }

    let staging_dir = root.join(format!("{cache_key}.staging-{}", std::process::id()));
    if staging_dir.exists() {
        let _ = fs::remove_dir_all(&staging_dir);
    }
    fs::create_dir_all(&staging_dir)?;

    match extract_native_libraries(libraries, &staging_dir) {
        Ok(()) => {
            fs::write(staging_dir.join(".ready"), b"ready")?;
        }
        Err(error) => {
            let _ = fs::remove_dir_all(&staging_dir);
            return Err(io::Error::other(error.to_string()));
        }
    }

    if ready_dir.exists() && !ready_marker.is_file() {
        let _ = fs::remove_dir_all(&ready_dir);
    }

    match fs::rename(&staging_dir, &ready_dir) {
        Ok(()) => Ok(ready_dir),
        Err(rename_error) => {
            if ready_marker.is_file() {
                let _ = fs::remove_dir_all(&staging_dir);
                Ok(ready_dir)
            } else {
                let _ = fs::remove_dir_all(&staging_dir);
                Err(rename_error)
            }
        }
    }
}

fn extract_native_libraries(
    libraries: &[ResolvedLibrary],
    natives_dir: &Path,
) -> Result<(), zip::result::ZipError> {
    for library in libraries {
        if !library.is_native || !library.abs_path.is_file() {
            continue;
        }

        let file = fs::File::open(&library.abs_path).map_err(zip::result::ZipError::Io)?;
        let mut archive = ZipArchive::new(file)?;
        for index in 0..archive.len() {
            let mut entry = archive.by_index(index)?;
            let name = entry.name().replace('\\', "/");
            if name.starts_with("META-INF/") || entry.is_dir() {
                continue;
            }

            let Some(file_name) = Path::new(&name).file_name() else {
                continue;
            };
            let dest_path = natives_dir.join(file_name);
            let mut out = fs::File::create(dest_path).map_err(zip::result::ZipError::Io)?;
            io::copy(&mut entry, &mut out).map_err(zip::result::ZipError::Io)?;
        }
    }
    Ok(())
}

fn natives_cache_root() -> io::Result<PathBuf> {
    let cache_dir = std::env::var_os(if cfg!(target_os = "windows") {
        "LOCALAPPDATA"
    } else {
        "XDG_CACHE_HOME"
    })
    .map(PathBuf::from)
    .or_else(|| {
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .map(|path| path.join(".cache"))
    })
    .or_else(|| std::env::current_dir().ok())
    .unwrap_or_else(|| PathBuf::from("."));

    Ok(cache_dir.join("axial").join("cache").join("natives"))
}

fn native_cache_key(version_id: &str, libraries: &[ResolvedLibrary]) -> String {
    let mut native_artifacts = libraries
        .iter()
        .filter(|library| library.is_native)
        .map(native_artifact_cache_key_part)
        .collect::<Vec<_>>();
    native_artifacts.sort();

    let mut seed = String::new();
    seed.push_str(version_id);
    seed.push('\n');
    for artifact in native_artifacts {
        seed.push_str(&artifact);
        seed.push('\n');
    }

    let digest = md5_compute(seed.as_bytes());
    format!("{version_id}-{:x}", digest)
}

fn native_artifact_cache_key_part(library: &ResolvedLibrary) -> String {
    let mut part = String::new();
    part.push_str(&library.name);
    part.push('\n');
    part.push_str(&library.abs_path.to_string_lossy());

    match fs::metadata(&library.abs_path) {
        Ok(metadata) => {
            part.push_str("\nfile=");
            part.push_str(if metadata.is_file() { "1" } else { "0" });
            part.push_str("\nlen=");
            part.push_str(&metadata.len().to_string());
            if let Ok(modified) = metadata.modified()
                && let Ok(duration) = modified.duration_since(std::time::UNIX_EPOCH)
            {
                part.push_str("\nmodified=");
                part.push_str(&duration.as_secs().to_string());
                part.push('.');
                part.push_str(&duration.subsec_nanos().to_string());
            }
        }
        Err(error) => {
            part.push_str("\nmissing=");
            part.push_str(error.kind().to_string().as_str());
        }
    }

    part
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn launch_plan_uses_instance_game_dir_but_shared_library_paths() {
        let root = std::env::temp_dir().join(format!(
            "axial-build-isolation-test-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("time")
                .as_nanos()
        ));
        let instance_dir = root.join("instances").join("survival");
        let version_dir = root.join("library").join("versions").join("test");
        fs::create_dir_all(&instance_dir).expect("instance dir");
        fs::create_dir_all(&version_dir).expect("version dir");
        fs::write(version_dir.join("test.jar"), b"jar").expect("client jar");

        let version: VersionJson = serde_json::from_value(serde_json::json!({
            "id": "test",
            "type": "release",
            "mainClass": "net.minecraft.client.main.Main",
            "arguments": {
                "game": [
                    "--gameDir",
                    "${game_directory}",
                    "--assetsDir",
                    "${assets_root}"
                ],
                "jvm": [
                    "-DlibraryDir=${library_directory}",
                    "-DassetRoot=${assets_root}",
                    "-cp",
                    "${classpath}"
                ]
            },
            "assetIndex": { "id": "test-assets" },
            "libraries": [{
                "name": "com.example:demo:1.0.0",
                "downloads": {
                    "artifact": {
                        "path": "com/example/demo/1.0.0/demo-1.0.0.jar",
                        "url": "https://example.invalid/demo-1.0.0.jar"
                    }
                }
            }]
        }))
        .expect("version json");
        write_test_asset_index(
            &root.join("library"),
            "test-assets",
            serde_json::json!({ "objects": {} }),
        );

        let library_dir = root.join("library");
        let plan = plan_resolved_launch(
            &VanillaLaunchRequest {
                session_id: "test-session".to_string(),
                mc_dir: library_dir.clone(),
                version_id: "test".to_string(),
                target_version_id: String::new(),
                auth: LaunchAuthContext::offline("Player"),
                runtime: RuntimeSelection {
                    requested_path: String::new(),
                    selected_path: String::new(),
                    selected_info: axial_minecraft::JavaRuntimeInfo {
                        id: String::new(),
                        major: 21,
                        update: 0,
                        distribution: "test".to_string(),
                        path: String::new(),
                    },
                    effective_path: "/usr/bin/java".to_string(),
                    effective_info: axial_minecraft::JavaRuntimeInfo {
                        id: "java".to_string(),
                        major: 21,
                        update: 0,
                        distribution: "test".to_string(),
                        path: "/usr/bin/java".to_string(),
                    },
                    effective_source: "managed".to_string(),
                    bypassed_requested_runtime: false,
                },
                game_dir: Some(instance_dir.clone()),
                launcher_name: "axial".to_string(),
                launcher_version: "test".to_string(),
                min_memory_mb: None,
                max_memory_mb: None,
                extra_jvm_args: Vec::new(),
                resolution: None,
            },
            version,
        )
        .expect("launch plan");

        assert_eq!(plan.game_dir, instance_dir);
        assert!(
            plan.game_args.windows(2).any(|args| args[0] == "--gameDir"
                && args[1] == plan.game_dir.to_string_lossy().as_ref())
        );
        assert!(
            plan.game_args
                .windows(2)
                .any(|args| args[0] == "--assetsDir"
                    && args[1] == library_dir.join("assets").to_string_lossy().as_ref())
        );
        assert!(
            plan.classpath.contains(
                &library_dir
                    .join("libraries")
                    .join("com/example/demo/1.0.0/demo-1.0.0.jar")
                    .to_string_lossy()
                    .to_string()
            )
        );
        assert!(plan.jvm_args.iter().any(|arg| {
            arg == &format!(
                "-DlibraryDir={}",
                library_dir.join("libraries").to_string_lossy()
            )
        }));
        assert!(plan.jvm_args.iter().any(|arg| {
            arg == &format!(
                "-DassetRoot={}",
                library_dir.join("assets").to_string_lossy()
            )
        }));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn offline_auth_context_preserves_current_command_auth_args() {
        let root = unique_temp_root("axial-build-offline-auth-test");
        let version: VersionJson = auth_version_json();

        let plan = plan_resolved_launch(
            &VanillaLaunchRequest {
                session_id: "test-session".to_string(),
                mc_dir: root.clone(),
                version_id: "auth-test".to_string(),
                target_version_id: String::new(),
                auth: LaunchAuthContext::offline("Player"),
                runtime: test_runtime(),
                game_dir: None,
                launcher_name: "axial".to_string(),
                launcher_version: "test".to_string(),
                min_memory_mb: None,
                max_memory_mb: None,
                extra_jvm_args: Vec::new(),
                resolution: None,
            },
            version,
        )
        .expect("launch plan");

        assert_arg_value(&plan.game_args, "--username", "Player");
        assert_arg_value(&plan.game_args, "--uuid", &offline_uuid("Player"));
        assert_arg_value(&plan.game_args, "--accessToken", "0");
        assert_arg_value(&plan.game_args, "--clientId", "");
        assert_arg_value(&plan.game_args, "--xuid", "");
        assert_arg_value(&plan.game_args, "--userType", "msa");
        assert!(plan.command.iter().any(|arg| arg == "--accessToken"));
        assert!(plan.command.iter().any(|arg| arg == "0"));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn launch_plan_carries_legacy_asset_repair_verdict() {
        let root = unique_temp_root("axial-build-legacy-assets");
        write_test_asset_index(
            &root,
            "test-assets",
            serde_json::json!({ "objects": {}, "virtual": true }),
        );

        let plan = plan_resolved_launch(
            &test_launch_request(&root, "auth-test"),
            asset_version_json(),
        )
        .expect("legacy asset launch plan");

        assert!(plan.requires_virtual_asset_repair);
        assert_arg_value(
            &plan.game_args,
            "--gameAssets",
            &root
                .join("assets")
                .join("virtual")
                .join("legacy")
                .to_string_lossy(),
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn launch_plan_rejects_malformed_asset_flags() {
        let root = unique_temp_root("axial-build-malformed-assets");
        write_test_asset_index(
            &root,
            "test-assets",
            serde_json::json!({ "objects": {}, "virtual": "yes" }),
        );

        assert!(matches!(
            plan_resolved_launch(
                &test_launch_request(&root, "auth-test"),
                asset_version_json(),
            ),
            Err(VanillaLaunchPlanError::AssetIndexFlags(
                axial_minecraft::AssetIndexFlagsError::Parse(_)
            ))
        ));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn offline_authlib_multiplayer_override_applies_to_1_16_5() {
        let root = unique_temp_root("axial-build-offline-authlib-mp-test");
        let version: VersionJson = auth_version_json();

        let plan = plan_resolved_launch(
            &VanillaLaunchRequest {
                session_id: "test-session".to_string(),
                mc_dir: root.clone(),
                version_id: "fabric-loader-0.16.10-1.16.5".to_string(),
                target_version_id: "1.16.5".to_string(),
                auth: LaunchAuthContext::offline("Player"),
                runtime: test_runtime(),
                game_dir: None,
                launcher_name: "axial".to_string(),
                launcher_version: "test".to_string(),
                min_memory_mb: None,
                max_memory_mb: None,
                extra_jvm_args: vec!["-Dextra=true".to_string()],
                resolution: None,
            },
            version,
        )
        .expect("launch plan");

        for expected in AUTHLIB_OFFLINE_MULTIPLAYER_JVM_ARGS {
            assert!(
                plan.jvm_args.iter().any(|arg| arg == expected),
                "expected {expected} in {:?}",
                plan.jvm_args
            );
        }
        let override_index = plan
            .jvm_args
            .iter()
            .position(|arg| arg == AUTHLIB_OFFLINE_MULTIPLAYER_JVM_ARGS[0])
            .expect("override arg");
        let extra_index = plan
            .jvm_args
            .iter()
            .position(|arg| arg == "-Dextra=true")
            .expect("extra arg");
        assert!(override_index < extra_index);

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn offline_authlib_multiplayer_override_applies_to_direct_1_16_4() {
        let root = unique_temp_root("axial-build-offline-authlib-direct-test");
        let mut version: VersionJson = auth_version_json();
        version.id = "1.16.4".to_string();

        let plan = plan_resolved_launch(
            &VanillaLaunchRequest {
                session_id: "test-session".to_string(),
                mc_dir: root.clone(),
                version_id: "1.16.4".to_string(),
                target_version_id: String::new(),
                auth: LaunchAuthContext::offline("Player"),
                runtime: test_runtime(),
                game_dir: None,
                launcher_name: "axial".to_string(),
                launcher_version: "test".to_string(),
                min_memory_mb: None,
                max_memory_mb: None,
                extra_jvm_args: Vec::new(),
                resolution: None,
            },
            version,
        )
        .expect("launch plan");

        assert!(
            plan.jvm_args
                .iter()
                .any(|arg| arg == AUTHLIB_OFFLINE_MULTIPLAYER_JVM_ARGS[0])
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn offline_authlib_multiplayer_override_is_exact_and_offline_only() {
        let root = unique_temp_root("axial-build-offline-authlib-scope-test");
        let version: VersionJson = auth_version_json();
        let online_auth = LaunchAuthContext {
            player_name: "OnlinePlayer".to_string(),
            uuid: "11112222333344445555666677778888".to_string(),
            access_token: "test-access-token".to_string(),
            client_id: String::new(),
            xuid: String::new(),
            user_type: "msa".to_string(),
        };

        let online_plan = plan_resolved_launch(
            &VanillaLaunchRequest {
                session_id: "test-session".to_string(),
                mc_dir: root.clone(),
                version_id: "1.16.5".to_string(),
                target_version_id: "1.16.5".to_string(),
                auth: online_auth,
                runtime: test_runtime(),
                game_dir: None,
                launcher_name: "axial".to_string(),
                launcher_version: "test".to_string(),
                min_memory_mb: None,
                max_memory_mb: None,
                extra_jvm_args: Vec::new(),
                resolution: None,
            },
            version.clone(),
        )
        .expect("online launch plan");
        assert!(
            !online_plan
                .jvm_args
                .iter()
                .any(|arg| arg == AUTHLIB_OFFLINE_MULTIPLAYER_JVM_ARGS[0])
        );

        for adjacent_version in ["1.16.3", "1.17.1"] {
            let adjacent_plan = plan_resolved_launch(
                &VanillaLaunchRequest {
                    session_id: "test-session".to_string(),
                    mc_dir: root.clone(),
                    version_id: adjacent_version.to_string(),
                    target_version_id: adjacent_version.to_string(),
                    auth: LaunchAuthContext::offline("Player"),
                    runtime: test_runtime(),
                    game_dir: None,
                    launcher_name: "axial".to_string(),
                    launcher_version: "test".to_string(),
                    min_memory_mb: None,
                    max_memory_mb: None,
                    extra_jvm_args: Vec::new(),
                    resolution: None,
                },
                version.clone(),
            )
            .expect("adjacent launch plan");
            assert!(
                !adjacent_plan
                    .jvm_args
                    .iter()
                    .any(|arg| arg == AUTHLIB_OFFLINE_MULTIPLAYER_JVM_ARGS[0]),
                "unexpected authlib override for {adjacent_version}"
            );
        }

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn legacy_forge_1_5_2_uses_archived_fml_library_mirror() {
        let root = unique_temp_root("axial-build-legacy-fml-mirror");
        let version_dir = root.join("versions").join("1.5.2-forge-7.8.1.738");
        fs::create_dir_all(&version_dir).expect("version dir");
        fs::write(version_dir.join("1.5.2-forge-7.8.1.738.jar"), b"client").expect("client jar");
        let version: VersionJson = serde_json::from_value(serde_json::json!({
            "id": "1.5.2-forge-7.8.1.738",
            "type": "release",
            "mainClass": "net.minecraft.launchwrapper.Launch",
            "minecraftArguments": "${auth_player_name} ${auth_session}",
            "assetIndex": { "id": "pre-1.6" },
            "libraries": [
                { "name": "net.minecraftforge:legacyfixer:1.0" },
                { "name": "net.minecraftforge:forge:1.5.2-7.8.1.738:universal" }
            ]
        }))
        .expect("version json");
        write_test_asset_index(
            &root,
            "pre-1.6",
            serde_json::json!({ "objects": {}, "virtual": true }),
        );

        let plan = plan_resolved_launch(
            &VanillaLaunchRequest {
                session_id: "test-session".to_string(),
                mc_dir: root.clone(),
                version_id: "1.5.2-forge-7.8.1.738".to_string(),
                target_version_id: "1.5.2".to_string(),
                auth: LaunchAuthContext::offline("Player"),
                runtime: test_runtime(),
                game_dir: None,
                launcher_name: "axial".to_string(),
                launcher_version: "test".to_string(),
                min_memory_mb: None,
                max_memory_mb: None,
                extra_jvm_args: Vec::new(),
                resolution: None,
            },
            version,
        )
        .expect("launch plan");

        assert!(
            plan.jvm_args
                .iter()
                .any(|arg| arg == LEGACY_FML_LIBRARY_MIRROR_JVM_ARG),
            "expected legacy FML mirror arg in {:?}",
            plan.jvm_args
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn legacy_fml_library_mirror_is_scoped_to_legacyfixer_1_5_versions() {
        let root = unique_temp_root("axial-build-legacy-fml-mirror-scope");
        let version: VersionJson = auth_version_json();
        let vanilla_plan = plan_resolved_launch(
            &VanillaLaunchRequest {
                session_id: "test-session".to_string(),
                mc_dir: root.clone(),
                version_id: "1.5.2".to_string(),
                target_version_id: "1.5.2".to_string(),
                auth: LaunchAuthContext::offline("Player"),
                runtime: test_runtime(),
                game_dir: None,
                launcher_name: "axial".to_string(),
                launcher_version: "test".to_string(),
                min_memory_mb: None,
                max_memory_mb: None,
                extra_jvm_args: Vec::new(),
                resolution: None,
            },
            version.clone(),
        )
        .expect("vanilla launch plan");
        assert!(
            !vanilla_plan
                .jvm_args
                .iter()
                .any(|arg| arg == LEGACY_FML_LIBRARY_MIRROR_JVM_ARG)
        );

        let mut modern_forge = version;
        modern_forge.id = "1.6.4-forge-9.11.1.1345".to_string();
        modern_forge
            .libraries
            .push(axial_minecraft::launch::Library {
                name: "net.minecraftforge:legacyfixer:1.0".to_string(),
                ..axial_minecraft::launch::Library::default()
            });
        let modern_plan = plan_resolved_launch(
            &VanillaLaunchRequest {
                session_id: "test-session".to_string(),
                mc_dir: root.clone(),
                version_id: "1.6.4-forge-9.11.1.1345".to_string(),
                target_version_id: "1.6.4".to_string(),
                auth: LaunchAuthContext::offline("Player"),
                runtime: test_runtime(),
                game_dir: None,
                launcher_name: "axial".to_string(),
                launcher_version: "test".to_string(),
                min_memory_mb: None,
                max_memory_mb: None,
                extra_jvm_args: Vec::new(),
                resolution: None,
            },
            modern_forge,
        )
        .expect("modern launch plan");
        assert!(
            !modern_plan
                .jvm_args
                .iter()
                .any(|arg| arg == LEGACY_FML_LIBRARY_MIRROR_JVM_ARG)
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn launch_auth_context_debug_redacts_access_token() {
        let auth = LaunchAuthContext {
            player_name: "OnlinePlayer".to_string(),
            uuid: "11112222333344445555666677778888".to_string(),
            access_token: "test-access-token".to_string(),
            client_id: "test-client-id".to_string(),
            xuid: "test-xuid".to_string(),
            user_type: "msa".to_string(),
        };

        let debug = format!("{auth:?}");

        assert!(!debug.contains("test-access-token"), "{debug}");
        assert!(debug.contains(r#"access_token: "[redacted]""#), "{debug}");
    }

    #[test]
    fn explicit_auth_context_flows_into_launch_variables_and_command() {
        let root = unique_temp_root("axial-build-explicit-auth-test");
        let version: VersionJson = auth_version_json();
        let auth = LaunchAuthContext {
            player_name: "OnlinePlayer".to_string(),
            uuid: "11112222333344445555666677778888".to_string(),
            access_token: "test-access-token".to_string(),
            client_id: "test-client-id".to_string(),
            xuid: "test-xuid".to_string(),
            user_type: "msa".to_string(),
        };

        let plan = plan_resolved_launch(
            &VanillaLaunchRequest {
                session_id: "test-session".to_string(),
                mc_dir: root.clone(),
                version_id: "auth-test".to_string(),
                target_version_id: String::new(),
                auth,
                runtime: test_runtime(),
                game_dir: None,
                launcher_name: "axial".to_string(),
                launcher_version: "test".to_string(),
                min_memory_mb: None,
                max_memory_mb: None,
                extra_jvm_args: Vec::new(),
                resolution: None,
            },
            version,
        )
        .expect("launch plan");

        assert_arg_value(&plan.game_args, "--username", "OnlinePlayer");
        assert_arg_value(
            &plan.game_args,
            "--uuid",
            "11112222333344445555666677778888",
        );
        assert_arg_value(&plan.game_args, "--accessToken", "test-access-token");
        assert_arg_value(&plan.game_args, "--clientId", "test-client-id");
        assert_arg_value(&plan.game_args, "--xuid", "test-xuid");
        assert_arg_value(&plan.game_args, "--userType", "msa");
        assert!(
            plan.jvm_args
                .iter()
                .any(|arg| arg == "-Dauth.client=test-client-id")
        );
        assert!(plan.command.iter().any(|arg| arg == "test-access-token"));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn modern_native_libraries_get_explicit_native_and_temp_paths() {
        let root = std::env::temp_dir().join(format!(
            "axial-build-test-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("time")
                .as_nanos()
        ));
        let version_dir = root.join("versions").join("test");
        let library_dir = root
            .join("libraries")
            .join("org")
            .join("lwjgl")
            .join("lwjgl")
            .join("3.3.3");

        fs::create_dir_all(&version_dir).expect("version dir");
        fs::create_dir_all(&library_dir).expect("library dir");
        fs::write(version_dir.join("test.jar"), b"jar").expect("client jar");
        fs::write(
            version_dir.join("test.json"),
            serde_json::json!({
                "id": "test",
                "type": "release",
                "mainClass": "net.minecraft.client.main.Main",
                "arguments": {
                    "game": [],
                    "jvm": ["-cp", "${classpath}"]
                },
                "assetIndex": { "id": "test-assets" },
                "libraries": [{
                    "name": "org.lwjgl:lwjgl:3.3.3:natives-linux",
                    "downloads": {
                        "artifact": {
                            "path": "org/lwjgl/lwjgl/3.3.3/lwjgl-3.3.3-natives-linux.jar",
                            "url": "https://libraries.minecraft.net/org/lwjgl/lwjgl/3.3.3/lwjgl-3.3.3-natives-linux.jar"
                        }
                    }
                }]
            })
            .to_string(),
        )
        .expect("version json");
        write_test_asset_index(&root, "test-assets", serde_json::json!({ "objects": {} }));

        let native_jar = library_dir.join("lwjgl-3.3.3-natives-linux.jar");
        let file = fs::File::create(&native_jar).expect("native jar");
        let mut zip = zip::ZipWriter::new(file);
        let options = zip::write::SimpleFileOptions::default();
        zip.start_file("liblwjgl.so", options)
            .expect("start native entry");
        use std::io::Write as _;
        zip.write_all(b"native").expect("write native entry");
        zip.finish().expect("finish native jar");

        let plan = plan_vanilla_launch(&VanillaLaunchRequest {
            session_id: "test-session".to_string(),
            mc_dir: root.clone(),
            version_id: "test".to_string(),
            target_version_id: String::new(),
            auth: LaunchAuthContext::offline("Player"),
            runtime: RuntimeSelection {
                requested_path: String::new(),
                selected_path: String::new(),
                selected_info: axial_minecraft::JavaRuntimeInfo {
                    id: String::new(),
                    major: 21,
                    update: 0,
                    distribution: "test".to_string(),
                    path: String::new(),
                },
                effective_path: "/usr/bin/java".to_string(),
                effective_info: axial_minecraft::JavaRuntimeInfo {
                    id: "java".to_string(),
                    major: 21,
                    update: 0,
                    distribution: "test".to_string(),
                    path: "/usr/bin/java".to_string(),
                },
                effective_source: "managed".to_string(),
                bypassed_requested_runtime: false,
            },
            game_dir: None,
            launcher_name: "axial".to_string(),
            launcher_version: "test".to_string(),
            min_memory_mb: None,
            max_memory_mb: None,
            extra_jvm_args: Vec::new(),
            resolution: None,
        })
        .expect("launch plan");

        assert!(plan.natives_dir.is_some());
        let natives_dir = plan.natives_dir.as_ref().expect("natives dir");
        assert!(natives_dir.to_string_lossy().contains(&format!(
            "axial{}cache{}natives",
            std::path::MAIN_SEPARATOR,
            std::path::MAIN_SEPARATOR
        )));
        assert!(
            plan.jvm_args
                .iter()
                .any(|arg| arg.starts_with("-Dorg.lwjgl.librarypath="))
        );
        assert!(
            plan.jvm_args
                .iter()
                .any(|arg| arg.starts_with("-Dorg.lwjgl.system.SharedLibraryExtractPath="))
        );
        assert!(
            plan.jvm_args
                .iter()
                .any(|arg| arg.starts_with("-Djna.tmpdir="))
        );
        assert!(
            plan.jvm_args
                .iter()
                .any(|arg| arg.starts_with("-Djava.io.tmpdir="))
        );

        let _ = fs::remove_dir_all(root);
        let _ = cleanup_natives_dir(natives_dir);
    }

    #[test]
    fn native_cache_key_changes_when_native_artifact_changes() {
        let root = unique_temp_root("axial-build-native-cache-key-test");
        fs::create_dir_all(&root).expect("root");
        let native_jar = root.join("demo-natives.jar");
        write_native_zip(&native_jar, b"native-v1").expect("write native v1");
        let libraries = vec![ResolvedLibrary {
            abs_path: native_jar.clone(),
            is_native: true,
            name: "org.lwjgl:lwjgl:3.3.3:natives-linux".to_string(),
        }];

        let first_key = native_cache_key("test", &libraries);
        write_native_zip(&native_jar, b"native-v2-with-different-size").expect("write native v2");
        let second_key = native_cache_key("test", &libraries);

        assert_ne!(first_key, second_key);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn find_client_jar_prefers_child_jar_over_parent_jar() {
        let root = unique_temp_root("axial-build-child-jar-preference");
        let versions_dir = root.join("versions");
        fs::create_dir_all(versions_dir.join("base")).expect("base dir");
        fs::create_dir_all(versions_dir.join("child")).expect("child dir");
        fs::write(versions_dir.join("base").join("base.jar"), b"base").expect("base jar");
        let child_jar = versions_dir.join("child").join("child.jar");
        fs::write(&child_jar, b"child").expect("child jar");
        write_test_version_json(
            &root,
            "child",
            serde_json::json!({
                "id": "child",
                "inheritsFrom": "base",
                "downloads": {
                    "client": { "url": "https://example.invalid/child.jar" }
                }
            }),
        );
        let version: VersionJson = serde_json::from_value(serde_json::json!({
            "id": "child",
            "downloads": {
                "client": { "url": "https://example.invalid/child.jar" }
            }
        }))
        .expect("version");

        let selected = find_client_jar(&root, &version, "child").expect("client jar");

        assert_eq!(selected, child_jar);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn find_client_jar_falls_back_to_parent_when_child_has_no_client_artifact() {
        let root = unique_temp_root("axial-build-parent-jar-fallback");
        let versions_dir = root.join("versions");
        fs::create_dir_all(versions_dir.join("base")).expect("base dir");
        fs::create_dir_all(versions_dir.join("child")).expect("child dir");
        let parent_jar = versions_dir.join("base").join("base.jar");
        fs::write(&parent_jar, b"base").expect("base jar");
        write_test_version_json(
            &root,
            "child",
            serde_json::json!({
                "id": "child",
                "inheritsFrom": "base"
            }),
        );
        let version: VersionJson =
            serde_json::from_value(serde_json::json!({ "id": "child" })).expect("version");

        let selected = find_client_jar(&root, &version, "child").expect("client jar");

        assert_eq!(selected, parent_jar);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn find_client_jar_does_not_fall_back_to_parent_when_child_client_is_missing() {
        let root = unique_temp_root("axial-build-no-parent-mask-child-client");
        let versions_dir = root.join("versions");
        fs::create_dir_all(versions_dir.join("base")).expect("base dir");
        fs::create_dir_all(versions_dir.join("child")).expect("child dir");
        fs::write(versions_dir.join("base").join("base.jar"), b"base").expect("base jar");
        write_test_version_json(
            &root,
            "child",
            serde_json::json!({
                "id": "child",
                "inheritsFrom": "base",
                "downloads": {
                    "client": { "url": "https://example.invalid/child.jar" }
                }
            }),
        );
        let version: VersionJson = serde_json::from_value(serde_json::json!({
            "id": "child",
            "downloads": {
                "client": { "url": "https://example.invalid/child.jar" }
            }
        }))
        .expect("version");

        assert_eq!(find_client_jar(&root, &version, "child"), None);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn module_bootstrap_launch_keeps_game_jar_in_classpath() {
        let root = unique_temp_root("axial-build-module-bootstrap-classpath");
        let version_id = "1.20.1-forge-47.4.20";
        let version_dir = root.join("versions").join(version_id);
        fs::create_dir_all(&version_dir).expect("version dir");
        let game_jar = version_dir.join(format!("{version_id}.jar"));
        fs::write(&game_jar, b"patched client").expect("game jar");
        write_test_version_json(
            &root,
            version_id,
            serde_json::json!({
                "id": version_id,
                "type": "release",
                "mainClass": "cpw.mods.bootstraplauncher.BootstrapLauncher",
                "assetIndex": { "id": "1.20.1" },
                "arguments": {
                    "game": [],
                    "jvm": [
                        "-p",
                        "${classpath}",
                        "--add-modules",
                        "ALL-MODULE-PATH"
                    ]
                },
                "libraries": []
            }),
        );

        let plan = plan_vanilla_launch(&VanillaLaunchRequest {
            session_id: "test-session".to_string(),
            mc_dir: root.clone(),
            version_id: version_id.to_string(),
            target_version_id: String::new(),
            auth: LaunchAuthContext::offline("Player"),
            runtime: test_runtime(),
            game_dir: None,
            launcher_name: "axial".to_string(),
            launcher_version: "test".to_string(),
            min_memory_mb: None,
            max_memory_mb: None,
            extra_jvm_args: Vec::new(),
            resolution: None,
        })
        .expect("launch plan");

        assert_eq!(plan.client_jar_path.as_deref(), Some(game_jar.as_path()));
        assert!(
            plan.classpath
                .contains(&game_jar.to_string_lossy().to_string())
        );
        assert!(
            plan.jvm_args
                .iter()
                .any(|arg| arg.contains(&game_jar.to_string_lossy().to_string()))
        );

        let _ = fs::remove_dir_all(root);
    }

    fn auth_version_json() -> VersionJson {
        serde_json::from_value(serde_json::json!({
            "id": "auth-test",
            "type": "release",
            "mainClass": "net.minecraft.client.main.Main",
            "arguments": {
                "game": [
                    "--username",
                    "${auth_player_name}",
                    "--uuid",
                    "${auth_uuid}",
                    "--accessToken",
                    "${auth_access_token}",
                    "--clientId",
                    "${clientid}",
                    "--xuid",
                    "${auth_xuid}",
                    "--userType",
                    "${user_type}"
                ],
                "jvm": [
                    "-Dauth.client=${clientid}"
                ]
            },
            "assetIndex": {},
            "libraries": []
        }))
        .expect("version json")
    }

    fn asset_version_json() -> VersionJson {
        let mut version = auth_version_json();
        version.asset_index.id = "test-assets".to_string();
        version.arguments = None;
        version.minecraft_arguments = "--gameAssets ${game_assets}".to_string();
        version
    }

    fn assert_arg_value(args: &[String], name: &str, expected: &str) {
        assert!(
            args.windows(2)
                .any(|pair| pair[0] == name && pair[1] == expected),
            "expected {name} to be followed by {expected:?} in {args:?}"
        );
    }

    fn test_runtime() -> RuntimeSelection {
        RuntimeSelection {
            requested_path: String::new(),
            selected_path: String::new(),
            selected_info: axial_minecraft::JavaRuntimeInfo {
                id: String::new(),
                major: 21,
                update: 0,
                distribution: "test".to_string(),
                path: String::new(),
            },
            effective_path: "/usr/bin/java".to_string(),
            effective_info: axial_minecraft::JavaRuntimeInfo {
                id: "java".to_string(),
                major: 21,
                update: 0,
                distribution: "test".to_string(),
                path: "/usr/bin/java".to_string(),
            },
            effective_source: "managed".to_string(),
            bypassed_requested_runtime: false,
        }
    }

    fn test_launch_request(root: &Path, version_id: &str) -> VanillaLaunchRequest {
        VanillaLaunchRequest {
            session_id: "test-session".to_string(),
            mc_dir: root.to_path_buf(),
            version_id: version_id.to_string(),
            target_version_id: String::new(),
            auth: LaunchAuthContext::offline("Player"),
            runtime: test_runtime(),
            game_dir: None,
            launcher_name: "axial".to_string(),
            launcher_version: "test".to_string(),
            min_memory_mb: None,
            max_memory_mb: None,
            extra_jvm_args: Vec::new(),
            resolution: None,
        }
    }

    fn unique_temp_root(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "{label}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("time")
                .as_nanos()
        ))
    }

    fn write_test_version_json(root: &Path, version_id: &str, value: serde_json::Value) {
        if let Some(asset_index_id) = value
            .get("assetIndex")
            .and_then(|asset_index| asset_index.get("id"))
            .and_then(serde_json::Value::as_str)
            .filter(|asset_index_id| !asset_index_id.is_empty())
        {
            write_test_asset_index(root, asset_index_id, serde_json::json!({ "objects": {} }));
        }
        let version_dir = root.join("versions").join(version_id);
        fs::create_dir_all(&version_dir).expect("version dir");
        fs::write(
            version_dir.join(format!("{version_id}.json")),
            value.to_string(),
        )
        .expect("version json");
    }

    fn write_test_asset_index(root: &Path, asset_index_id: &str, value: serde_json::Value) {
        let indexes_dir = root.join("assets").join("indexes");
        fs::create_dir_all(&indexes_dir).expect("asset indexes directory");
        fs::write(
            indexes_dir.join(format!("{asset_index_id}.json")),
            value.to_string(),
        )
        .expect("asset index");
    }

    fn write_native_zip(path: &Path, contents: &[u8]) -> io::Result<()> {
        let file = fs::File::create(path)?;
        let mut zip = zip::ZipWriter::new(file);
        let options = zip::write::SimpleFileOptions::default();
        zip.start_file("libdemo.so", options)?;
        use std::io::Write as _;
        zip.write_all(contents)?;
        zip.finish()?;
        Ok(())
    }
}
