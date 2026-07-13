use crate::error::{ContentError, ContentResult};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static TRANSACTION_SEQUENCE: AtomicU64 = AtomicU64::new(1);

pub(crate) fn staging_dir(root: &Path, prefix: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let sequence = TRANSACTION_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    root.join(format!(".{prefix}-{nanos:x}-{sequence:x}"))
}

pub(crate) struct StagingGuard {
    path: PathBuf,
    transferred: bool,
}

impl StagingGuard {
    pub(crate) fn create(root: &Path, prefix: &str) -> ContentResult<Self> {
        let path = staging_dir(root, prefix);
        fs::create_dir_all(&path)?;
        Ok(Self {
            path,
            transferred: false,
        })
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn transfer(mut self) -> PathBuf {
        self.transferred = true;
        self.path.clone()
    }
}

impl Drop for StagingGuard {
    fn drop(&mut self) {
        if !self.transferred {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

pub(crate) fn contained_path(root: &Path, relative: &str) -> ContentResult<PathBuf> {
    let candidate = Path::new(relative);
    if candidate.is_absolute() {
        return Err(ContentError::Invalid(format!(
            "content file escapes the instance: {relative}"
        )));
    }
    let mut resolved = root.to_path_buf();
    for component in candidate.components() {
        match component {
            Component::Normal(part) => resolved.push(part),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(ContentError::Invalid(format!(
                    "content file escapes the instance: {relative}"
                )));
            }
        }
    }
    Ok(resolved)
}

pub(crate) struct FileTransaction {
    root: PathBuf,
    staging: PathBuf,
    backup: PathBuf,
    applied: Vec<(String, bool)>,
    finished: bool,
}

impl FileTransaction {
    pub(crate) fn apply(
        root: &Path,
        staging: PathBuf,
        relative_paths: &[String],
    ) -> ContentResult<Self> {
        let backup = staging.join(".backup");
        let mut transaction = Self {
            root: root.to_path_buf(),
            staging,
            backup,
            applied: Vec::new(),
            finished: false,
        };
        for relative in relative_paths {
            if let Err(error) = transaction.apply_one(relative) {
                transaction.rollback_inner();
                transaction.finished = true;
                return Err(error);
            }
        }
        Ok(transaction)
    }

    fn apply_one(&mut self, relative: &str) -> ContentResult<()> {
        let staged = contained_path(&self.staging, relative)?;
        let destination = contained_path(&self.root, relative)?;
        let backup = contained_path(&self.backup, relative)?;
        if destination.is_dir() {
            return Err(ContentError::Invalid(format!(
                "content destination is a directory: {relative}"
            )));
        }
        let existed = destination.exists();
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)?;
        }
        if existed {
            if let Some(parent) = backup.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::rename(&destination, &backup)?;
        }
        if let Err(error) = fs::rename(&staged, &destination) {
            if existed {
                let _ = fs::rename(&backup, &destination);
            }
            return Err(ContentError::Io(error));
        }
        self.applied.push((relative.to_string(), existed));
        Ok(())
    }

    pub(crate) fn commit(mut self) {
        self.finished = true;
        let _ = fs::remove_dir_all(&self.staging);
    }

    pub(crate) fn rollback(mut self) {
        self.rollback_inner();
        self.finished = true;
    }

    fn rollback_inner(&mut self) {
        for (relative, existed) in self.applied.iter().rev() {
            if let Ok(destination) = contained_path(&self.root, relative) {
                let _ = fs::remove_file(&destination);
                if *existed && let Ok(backup) = contained_path(&self.backup, relative) {
                    if let Some(parent) = destination.parent() {
                        let _ = fs::create_dir_all(parent);
                    }
                    let _ = fs::rename(backup, destination);
                }
            }
        }
        let _ = fs::remove_dir_all(&self.staging);
    }
}

impl Drop for FileTransaction {
    fn drop(&mut self) {
        if !self.finished {
            self.rollback_inner();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "axial-content-transaction-{name}-{}",
            TRANSACTION_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("create fixture root");
        root
    }

    #[test]
    fn rollback_restores_replaced_files() {
        let root = root("rollback");
        fs::create_dir_all(root.join("mods")).expect("mods");
        fs::write(root.join("mods/example.jar"), b"old").expect("old file");
        let staging = StagingGuard::create(&root, "stage").expect("stage");
        fs::create_dir_all(staging.path().join("mods")).expect("staged mods");
        fs::write(staging.path().join("mods/example.jar"), b"new").expect("new file");

        FileTransaction::apply(&root, staging.transfer(), &["mods/example.jar".to_string()])
            .expect("apply")
            .rollback();

        assert_eq!(
            fs::read(root.join("mods/example.jar")).expect("restored"),
            b"old"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn failed_apply_rolls_back_earlier_files() {
        let root = root("partial");
        fs::create_dir_all(root.join("mods")).expect("mods");
        fs::write(root.join("mods/first.jar"), b"old").expect("old file");
        let staging = StagingGuard::create(&root, "stage").expect("stage");
        fs::create_dir_all(staging.path().join("mods")).expect("staged mods");
        fs::write(staging.path().join("mods/first.jar"), b"new").expect("new file");

        let result = FileTransaction::apply(
            &root,
            staging.transfer(),
            &["mods/first.jar".to_string(), "mods/missing.jar".to_string()],
        );

        assert!(result.is_err());
        assert_eq!(
            fs::read(root.join("mods/first.jar")).expect("restored"),
            b"old"
        );
        assert!(!root.join("mods/missing.jar").exists());
        let _ = fs::remove_dir_all(root);
    }
}
