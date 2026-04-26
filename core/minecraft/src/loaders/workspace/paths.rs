use crate::paths::{loader_artifacts_dir, loader_catalog_dir, loader_work_dir};
use std::path::{Path, PathBuf};

pub fn catalog_dir(library_dir: &Path) -> PathBuf {
    loader_catalog_dir(library_dir)
}

pub fn artifacts_dir(library_dir: &Path) -> PathBuf {
    loader_artifacts_dir(library_dir)
}

pub fn work_dir(library_dir: &Path) -> PathBuf {
    loader_work_dir(library_dir)
}
