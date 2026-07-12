use crate::paths::loader_work_dir;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

pub(crate) fn prepare_fresh_work_dir(library_dir: &Path, version_id: &str) -> io::Result<PathBuf> {
    let mut components = Path::new(version_id).components();
    if !matches!(components.next(), Some(Component::Normal(_))) || components.next().is_some() {
        return Err(unsafe_workspace_error());
    }
    require_exact_directory(library_dir)?;
    let cache = library_dir.join("cache");
    create_exact_directory_if_missing(&cache)?;
    let loaders = cache.join("loaders");
    create_exact_directory_if_missing(&loaders)?;
    let work = loader_work_dir(library_dir);
    create_exact_directory_if_missing(&work)?;
    let stage = work.join(version_id);
    match fs::symlink_metadata(&stage) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
            fs::remove_dir_all(&stage)?;
        }
        Ok(_) => return Err(unsafe_workspace_error()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    fs::create_dir(&stage)?;
    require_exact_directory(&stage)?;
    Ok(stage)
}

pub(crate) fn remove_work_dir(path: &Path) {
    if fs::symlink_metadata(path)
        .is_ok_and(|metadata| metadata.is_dir() && !metadata.file_type().is_symlink())
    {
        let _ = fs::remove_dir_all(path);
    }
}

fn create_exact_directory_if_missing(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(_) => require_exact_directory(path),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            fs::create_dir(path)?;
            require_exact_directory(path)
        }
        Err(error) => Err(error),
    }
}

fn require_exact_directory(path: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.is_dir() && !metadata.file_type().is_symlink() {
        Ok(())
    } else {
        Err(unsafe_workspace_error())
    }
}

fn unsafe_workspace_error() -> io::Error {
    io::Error::other("loader workspace path is not an exact managed directory")
}

#[cfg(test)]
mod tests {
    use super::{prepare_fresh_work_dir, remove_work_dir};
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[cfg(unix)]
    #[test]
    fn fresh_workspace_rejects_symlinked_stage_without_outside_mutation() {
        let root = temp_dir("workspace-symlink-stage");
        let outside = temp_dir("workspace-symlink-outside");
        fs::create_dir_all(root.join("cache/loaders/work")).expect("work root");
        fs::create_dir_all(&outside).expect("outside root");
        let sentinel = outside.join("sentinel");
        fs::write(&sentinel, b"untouched").expect("sentinel");
        std::os::unix::fs::symlink(&outside, root.join("cache/loaders/work/version"))
            .expect("stage symlink");

        assert!(prepare_fresh_work_dir(&root, "version").is_err());
        assert_eq!(fs::read(&sentinel).expect("sentinel"), b"untouched");
        remove_work_dir(&root.join("cache/loaders/work/version"));
        assert_eq!(fs::read(&sentinel).expect("sentinel"), b"untouched");

        let _ = fs::remove_dir_all(root);
        let _ = fs::remove_dir_all(outside);
    }

    fn temp_dir(prefix: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|value| value.as_nanos())
            .unwrap_or_default();
        std::env::temp_dir().join(format!("axial-{prefix}-{nanos:x}"))
    }
}
