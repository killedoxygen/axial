use std::fs;
use std::path::Path;

pub fn remove_work_dir(path: &Path) {
    let _ = fs::remove_dir_all(path);
}
