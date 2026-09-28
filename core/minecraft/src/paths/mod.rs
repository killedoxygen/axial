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
