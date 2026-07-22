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

pub fn loader_cache_dir(mc_dir: &Path) -> PathBuf {
    cache_dir(mc_dir).join("loaders")
}

pub fn loader_catalog_dir(mc_dir: &Path) -> PathBuf {
    loader_cache_dir(mc_dir).join("catalog")
}
