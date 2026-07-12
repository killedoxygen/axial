use crate::loaders::managed_fs::ManagedDir;
use crate::loaders::types::LoaderError;
use crate::loaders::validate_version_id;
use std::path::Path;

pub(crate) struct LoaderWorkspace {
    directory: ManagedDir,
}

pub(crate) struct LoaderWorkspaceTemp {
    directory: ManagedDir,
}

impl LoaderWorkspace {
    pub(crate) fn path(&self) -> &Path {
        self.directory.path()
    }

    pub(crate) async fn write_exact(&self, name: &str, bytes: &[u8]) -> Result<(), LoaderError> {
        self.directory.write_exact(name, bytes).await
    }

    pub(crate) fn revalidate(&self) -> Result<(), LoaderError> {
        self.directory.revalidate()
    }

    pub(crate) fn create_temp(&self, name: &str) -> Result<LoaderWorkspaceTemp, LoaderError> {
        if let Some(stale) = self.directory.open_child_if_exists(name)? {
            stale.clear_owned_contents()?;
        }
        let directory = self.directory.open_or_create_child(name)?;
        Ok(LoaderWorkspaceTemp { directory })
    }

    pub(crate) fn cleanup(self) -> Result<(), LoaderError> {
        self.directory.clear_owned_contents()
    }
}

impl LoaderWorkspaceTemp {
    pub(crate) async fn write_relative_exact(
        &self,
        relative: &crate::artifact_path::ArtifactRelativePath,
        bytes: &[u8],
    ) -> Result<(), LoaderError> {
        self.directory.write_relative_exact(relative, bytes).await
    }

    pub(crate) fn cleanup(self) -> Result<(), LoaderError> {
        self.directory.clear_owned_contents()
    }
}

pub(crate) fn prepare_fresh_work_dir(
    library_dir: &Path,
    version_id: &str,
) -> Result<LoaderWorkspace, LoaderError> {
    validate_version_id(version_id, "installer workspace version id")?;
    let root = ManagedDir::open_root(library_dir)?;
    let work = root
        .open_or_create_child("cache")?
        .open_or_create_child("loaders")?
        .open_or_create_child("work")?;
    if let Some(stale) = work.open_child_if_exists(version_id)? {
        stale.clear_owned_contents()?;
    }
    let directory = work.open_or_create_child(version_id)?;
    directory.revalidate()?;
    Ok(LoaderWorkspace { directory })
}

#[cfg(test)]
mod tests {
    use super::prepare_fresh_work_dir;
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
        assert!(root.join("cache/loaders/work/version").is_symlink());
        assert_eq!(fs::read(&sentinel).expect("sentinel"), b"untouched");

        let _ = fs::remove_dir_all(root);
        let _ = fs::remove_dir_all(outside);
    }

    #[test]
    fn fresh_workspace_reuses_cleared_admitted_shell() {
        let root = temp_dir("workspace-retained-shell");
        fs::create_dir_all(&root).expect("root");
        let workspace = prepare_fresh_work_dir(&root, "version").expect("fresh workspace");
        fs::write(workspace.path().join("stale"), b"stale").expect("stale artifact");

        workspace.cleanup().expect("clear workspace");

        let stage = root.join("cache/loaders/work/version");
        assert!(stage.is_dir());
        assert_eq!(fs::read_dir(&stage).expect("cleared stage").count(), 0);
        let reused = prepare_fresh_work_dir(&root, "version").expect("reused workspace");
        assert_eq!(reused.path(), stage);
        drop(reused);
        let _ = fs::remove_dir_all(root);
    }

    fn temp_dir(prefix: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|value| value.as_nanos())
            .unwrap_or_default();
        std::env::temp_dir().join(format!("axial-{prefix}-{nanos:x}"))
    }
}
