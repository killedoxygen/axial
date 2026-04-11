pub mod steps;

use crate::runtime::RuntimeSelection;
use croopor_minecraft::{
    LaunchModelError, LaunchVars, ResolvedLibrary, VersionJson, build_classpath, client_jar_path,
    default_environment, is_legacy_assets, libraries_dir, offline_uuid, resolve_arguments,
    resolve_libraries, resolve_version,
};
use md5::compute as md5_compute;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use thiserror::Error;
use zip::ZipArchive;

#[derive(Debug, Clone)]
pub struct VanillaLaunchRequest {
    pub session_id: String,
    pub mc_dir: PathBuf,
    pub version_id: String,
    pub username: String,
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
    let client_jar = if uses_module_bootstrap(&version) {
        None
    } else {
        find_client_jar(&request.mc_dir, &version, &request.version_id)
    };

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
        let dir = create_natives_dir(&request.version_id, &libraries)?;
        if let Err(error) = extract_native_libraries(&libraries, &dir) {
            return Err(error.into());
        }
        Some(dir)
    } else {
        None
    };
    let game_assets = if !version.asset_index.id.is_empty()
        && is_legacy_assets(&request.mc_dir, &version.asset_index.id)
    {
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
        auth_player_name: request.username.clone(),
        version_name: version.id.clone(),
        game_directory: game_dir.to_string_lossy().to_string(),
        assets_root: request.mc_dir.join("assets").to_string_lossy().to_string(),
        asset_index_name: version.asset_index.id.clone(),
        auth_uuid: offline_uuid(&request.username),
        auth_access_token: "null".to_string(),
        client_id: String::new(),
        auth_xuid: String::new(),
        user_type: "legacy".to_string(),
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
    jvm_args.extend(request.extra_jvm_args.clone());
    let main_class = version.main_class.clone();

    let mut command = Vec::with_capacity(2 + jvm_args.len() + game_args.len());
    command.push(request.runtime.effective_path.clone());
    command.extend(jvm_args.clone());
    command.push(main_class.clone());
    command.extend(game_args.clone());

    Ok(VanillaLaunchPlan {
        version,
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

pub fn cleanup_natives_dir(dir: &Path) -> io::Result<()> {
    let cleaned = dir.to_string_lossy();
    let managed_legacy = format!(
        "{}croopor{}natives",
        std::path::MAIN_SEPARATOR,
        std::path::MAIN_SEPARATOR
    );
    let managed_cache = format!(
        "{}croopor{}cache{}natives",
        std::path::MAIN_SEPARATOR,
        std::path::MAIN_SEPARATOR,
        std::path::MAIN_SEPARATOR
    );
    if !cleaned.contains(&managed_legacy)
        && !cleaned.ends_with(&format!("croopor{}natives", std::path::MAIN_SEPARATOR))
        && !cleaned.contains(&managed_cache)
        && !cleaned.ends_with(&format!(
            "croopor{}cache{}natives",
            std::path::MAIN_SEPARATOR,
            std::path::MAIN_SEPARATOR
        ))
    {
        return Err(io::Error::other(format!(
            "refusing to remove non-croopor natives directory: {}",
            dir.display()
        )));
    }
    fs::remove_dir_all(dir)
}

fn find_client_jar(
    mc_dir: &Path,
    version: &VersionJson,
    original_version_id: &str,
) -> Option<PathBuf> {
    let versions_dir = mc_dir.join("versions");

    if !original_version_id.is_empty() {
        let original_json_path = versions_dir
            .join(original_version_id)
            .join(format!("{original_version_id}.json"));
        if let Ok(data) = fs::read_to_string(original_json_path) {
            #[derive(serde::Deserialize)]
            struct StubVersion {
                #[serde(rename = "inheritsFrom", default)]
                inherits_from: String,
            }

            if let Ok(stub) = serde_json::from_str::<StubVersion>(&data)
                && !stub.inherits_from.is_empty()
            {
                let parent_jar = versions_dir
                    .join(&stub.inherits_from)
                    .join(format!("{}.jar", stub.inherits_from));
                if parent_jar.is_file() {
                    return Some(parent_jar);
                }
            }
        }
    }

    let primary = client_jar_path(mc_dir, version, original_version_id);
    if primary.is_file() {
        return Some(primary);
    }

    let version_dir = versions_dir.join(&version.id);
    let Ok(entries) = fs::read_dir(version_dir) else {
        return None;
    };
    for entry in entries.flatten() {
        if entry.path().extension().is_some_and(|ext| ext == "jar") {
            return Some(entry.path());
        }
    }

    None
}

fn uses_module_bootstrap(version: &VersionJson) -> bool {
    let Some(arguments) = &version.arguments else {
        return false;
    };
    if version.main_class == "cpw.mods.bootstraplauncher.BootstrapLauncher" {
        return true;
    }

    let mut has_module_path = false;
    let mut has_all_module_path = false;
    for argument in &arguments.jvm {
        for value in &argument.value {
            match value.as_str() {
                "-p" | "--module-path" => has_module_path = true,
                "ALL-MODULE-PATH" => has_all_module_path = true,
                _ => {}
            }
        }
    }

    has_module_path && has_all_module_path
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

    Ok(cache_dir.join("croopor").join("cache").join("natives"))
}

fn native_cache_key(version_id: &str, libraries: &[ResolvedLibrary]) -> String {
    let mut native_paths = libraries
        .iter()
        .filter(|library| library.is_native)
        .map(|library| library.abs_path.to_string_lossy().to_string())
        .collect::<Vec<_>>();
    native_paths.sort();

    let mut seed = String::new();
    seed.push_str(version_id);
    seed.push('\n');
    for path in native_paths {
        seed.push_str(&path);
        seed.push('\n');
    }

    let digest = md5_compute(seed.as_bytes());
    format!("{version_id}-{:x}", digest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn modern_native_libraries_get_explicit_native_and_temp_paths() {
        let root = std::env::temp_dir().join(format!(
            "croopor-build-test-{}",
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
            username: "Player".to_string(),
            runtime: RuntimeSelection {
                requested_path: String::new(),
                selected_path: String::new(),
                selected_info: croopor_minecraft::JavaRuntimeInfo {
                    id: String::new(),
                    major: 21,
                    update: 0,
                    distribution: "test".to_string(),
                    path: String::new(),
                },
                effective_path: "/usr/bin/java".to_string(),
                effective_info: croopor_minecraft::JavaRuntimeInfo {
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
            launcher_name: "croopor".to_string(),
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
            "croopor{}cache{}natives",
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
}
