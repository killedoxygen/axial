use std::path::{Path, PathBuf};

pub fn libraries_dir(mc_dir: &Path) -> PathBuf {
    mc_dir.join("libraries")
}
