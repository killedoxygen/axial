use std::path::{Path, PathBuf};

pub fn assets_dir(mc_dir: &Path) -> PathBuf {
    mc_dir.join("assets")
}

pub fn libraries_dir(mc_dir: &Path) -> PathBuf {
    mc_dir.join("libraries")
}

pub fn versions_dir(mc_dir: &Path) -> PathBuf {
    mc_dir.join("versions")
}

pub fn cache_dir(mc_dir: &Path) -> PathBuf {
    mc_dir.join("cache")
}

pub fn version_manifest_cache_path(mc_dir: &Path) -> PathBuf {
    cache_dir(mc_dir).join("version_manifest_v2.json")
}

pub fn loader_cache_dir(mc_dir: &Path) -> PathBuf {
    cache_dir(mc_dir).join("loaders")
}

pub fn loader_catalog_dir(mc_dir: &Path) -> PathBuf {
    loader_cache_dir(mc_dir).join("catalog")
}

pub fn default_minecraft_dir() -> Option<PathBuf> {
    if cfg!(target_os = "windows") {
        std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .map(|path| path.join(".minecraft"))
    } else if cfg!(target_os = "macos") {
        std::env::var_os("HOME").map(PathBuf::from).map(|path| {
            path.join("Library")
                .join("Application Support")
                .join("minecraft")
        })
    } else {
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .map(|path| path.join(".minecraft"))
    }
}

pub fn runtime_dirs(mc_dir: &Path) -> Vec<PathBuf> {
    let mut dirs = vec![mc_dir.join("runtime")];
    if cfg!(target_os = "windows")
        && let Some(local_app_data) = std::env::var_os("LOCALAPPDATA")
    {
        dirs.push(
            PathBuf::from(local_app_data)
                .join("Packages")
                .join("Microsoft.4297127D64EC6_8wekyb3d8bbwe")
                .join("LocalCache")
                .join("Local")
                .join("runtime"),
        );
    }
    dirs
}

pub fn validate_installation(mc_dir: &Path) -> bool {
    ["versions", "libraries", "assets"]
        .iter()
        .all(|subdir| mc_dir.join(subdir).is_dir())
}

pub fn create_minecraft_dir(dir: &Path) -> std::io::Result<()> {
    for subdir in ["versions", "libraries", "assets", "cache/loaders/catalog"] {
        std::fs::create_dir_all(dir.join(subdir))?;
    }
    Ok(())
}
