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

pub fn create_minecraft_dir(dir: &Path) -> std::io::Result<()> {
    for subdir in ["versions", "libraries", "assets", "cache/loaders/catalog"] {
        std::fs::create_dir_all(dir.join(subdir))?;
    }
    Ok(())
}
