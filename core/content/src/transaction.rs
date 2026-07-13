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
    reject_symlink(root)?;
    let mut resolved = root.to_path_buf();
    for component in candidate.components() {
        match component {
            Component::Normal(part) => {
                resolved.push(part);
                reject_symlink(&resolved)?;
            }
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

fn reject_symlink(path: &Path) -> ContentResult<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(ContentError::Invalid(
            "content path contains a symbolic link".to_string(),
        )),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(ContentError::Io(error)),
    }
}

/// Promote a temporary file over an existing destination on every supported
/// platform. Windows rename does not replace files, so the old destination is
/// first moved aside and restored if promotion fails.
pub(crate) fn promote_replacement(source: &Path, destination: &Path) -> ContentResult<()> {
    let first_error = match fs::rename(source, destination) {
        Ok(()) => return Ok(()),
        Err(error) => error,
    };
    promote_replacement_after_rename_failure(source, destination, first_error)
}

fn promote_replacement_after_rename_failure(
    source: &Path,
    destination: &Path,
    first_error: std::io::Error,
) -> ContentResult<()> {
    match fs::symlink_metadata(source) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(ContentError::Io(first_error));
        }
        Err(error) => return Err(ContentError::Io(error)),
    }
    match fs::symlink_metadata(destination) {
        Ok(metadata) if metadata.is_file() || metadata.file_type().is_symlink() => {}
        Ok(_) => return Err(ContentError::Io(first_error)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(ContentError::Io(first_error));
        }
        Err(error) => return Err(ContentError::Io(error)),
    }

    let parent = destination.parent().unwrap_or_else(|| Path::new("."));
    let backup = staging_dir(parent, "axial-replacement-backup");
    fs::rename(destination, &backup)?;
    match fs::rename(source, destination) {
        Ok(()) => {
            let _ = fs::remove_file(backup);
            Ok(())
        }
        Err(error) => {
            let restore = fs::rename(&backup, destination);
            match restore {
                Ok(()) => Err(ContentError::Io(error)),
                Err(restore_error) => Err(ContentError::Io(std::io::Error::other(format!(
                    "failed to promote replacement: {error}; failed to restore destination: {restore_error}"
                )))),
            }
        }
    }
}

pub(crate) struct FileTransaction {
    root: PathBuf,
    staging: PathBuf,
    backup: PathBuf,
    applied: Vec<(String, bool)>,
    removed: Vec<String>,
    replace_existing: bool,
    finished: bool,
}

impl FileTransaction {
    pub(crate) fn apply(
        root: &Path,
        staging: PathBuf,
        relative_paths: &[String],
    ) -> ContentResult<Self> {
        Self::apply_with_policy(root, staging, relative_paths, true)
    }

    pub(crate) fn apply_new(
        root: &Path,
        staging: PathBuf,
        relative_paths: &[String],
    ) -> ContentResult<Self> {
        Self::apply_with_policy(root, staging, relative_paths, false)
    }

    fn apply_with_policy(
        root: &Path,
        staging: PathBuf,
        relative_paths: &[String],
        replace_existing: bool,
    ) -> ContentResult<Self> {
        let backup = staging.join(".backup");
        let mut transaction = Self {
            root: root.to_path_buf(),
            staging,
            backup,
            applied: Vec::new(),
            removed: Vec::new(),
            replace_existing,
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

    pub(crate) fn empty(root: &Path) -> ContentResult<Self> {
        let staging = StagingGuard::create(root, "axial-content-transaction")?;
        Self::apply(root, staging.transfer(), &[])
    }

    /// Move existing destinations into the transaction backup. Missing files
    /// are harmless, while every other filesystem error aborts the operation.
    /// Paths installed by this transaction are protected automatically.
    pub(crate) fn stage_removals(&mut self, relative_paths: &[String]) -> ContentResult<()> {
        for relative in relative_paths {
            self.stage_removal(relative)?;
        }
        Ok(())
    }

    fn stage_removal(&mut self, relative: &str) -> ContentResult<()> {
        if self.applied.iter().any(|(applied, _)| applied == relative)
            || self.removed.iter().any(|removed| removed == relative)
        {
            return Ok(());
        }
        let destination = contained_path(&self.root, relative)?;
        match fs::symlink_metadata(&destination) {
            Ok(metadata) if metadata.is_dir() => {
                return Err(ContentError::Invalid(format!(
                    "content destination is a directory: {relative}"
                )));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(ContentError::Io(error)),
        }
        let backup = contained_path(&self.backup, relative)?;
        if let Some(parent) = backup.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::rename(&destination, &backup)?;
        self.removed.push(relative.to_string());
        Ok(())
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
        let existed = match fs::symlink_metadata(&destination) {
            Ok(_) => true,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(error) => return Err(ContentError::Io(error)),
        };
        if existed && !self.replace_existing {
            return Err(ContentError::Invalid(
                "content destination became occupied before commit".to_string(),
            ));
        }
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
        for relative in self.removed.iter().rev() {
            if let (Ok(destination), Ok(backup)) = (
                contained_path(&self.root, relative),
                contained_path(&self.backup, relative),
            ) {
                if let Some(parent) = destination.parent() {
                    let _ = fs::create_dir_all(parent);
                }
                let _ = fs::rename(backup, destination);
            }
        }
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

    #[test]
    fn rollback_restores_staged_removals() {
        let root = root("remove-rollback");
        fs::create_dir_all(root.join("mods")).expect("mods");
        fs::write(root.join("mods/example.jar"), b"content").expect("content file");
        let mut transaction = FileTransaction::empty(&root).expect("transaction");

        transaction
            .stage_removals(&["mods/example.jar".to_string()])
            .expect("stage removal");
        assert!(!root.join("mods/example.jar").exists());
        transaction.rollback();

        assert_eq!(
            fs::read(root.join("mods/example.jar")).expect("restored"),
            b"content"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn staged_removal_preserves_new_transaction_destination() {
        let root = root("remove-new-destination");
        let staging = StagingGuard::create(&root, "stage").expect("stage");
        fs::create_dir_all(staging.path().join("mods")).expect("staged mods");
        fs::write(staging.path().join("mods/example.jar"), b"new").expect("new file");
        let mut transaction =
            FileTransaction::apply(&root, staging.transfer(), &["mods/example.jar".to_string()])
                .expect("apply");

        transaction
            .stage_removals(&["mods/example.jar".to_string()])
            .expect("protected removal");
        transaction.commit();

        assert_eq!(
            fs::read(root.join("mods/example.jar")).expect("installed"),
            b"new"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn new_file_transaction_refuses_an_occupied_destination() {
        let root = root("new-file-occupied");
        fs::create_dir_all(root.join("mods")).expect("mods");
        fs::write(root.join("mods/example.jar"), b"user file").expect("existing");
        let staging = StagingGuard::create(&root, "stage").expect("stage");
        fs::create_dir_all(staging.path().join("mods")).expect("staged mods");
        fs::write(staging.path().join("mods/example.jar"), b"pack file").expect("staged");

        let result = FileTransaction::apply_new(
            &root,
            staging.transfer(),
            &["mods/example.jar".to_string()],
        );

        assert!(result.is_err());
        assert_eq!(
            fs::read(root.join("mods/example.jar")).expect("preserved"),
            b"user file"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn replacement_promotion_replaces_an_existing_file() {
        let root = root("replace-existing");
        let source = root.join("manifest.tmp");
        let destination = root.join("manifest.json");
        fs::write(&source, b"new").expect("source");
        fs::write(&destination, b"old").expect("destination");

        promote_replacement(&source, &destination).expect("promote");

        assert_eq!(fs::read(&destination).expect("destination"), b"new");
        assert!(!source.exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn replacement_fallback_replaces_a_windows_style_existing_destination() {
        let root = root("replace-existing-fallback");
        let source = root.join("manifest.tmp");
        let destination = root.join("manifest.json");
        fs::write(&source, b"new").expect("source");
        fs::write(&destination, b"old").expect("destination");

        promote_replacement_after_rename_failure(
            &source,
            &destination,
            std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "simulated Windows replacement failure",
            ),
        )
        .expect("fallback promotion");

        assert_eq!(fs::read(&destination).expect("destination"), b"new");
        assert!(!source.exists());
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn contained_path_rejects_symlinked_ancestors() {
        use std::os::unix::fs::symlink;

        let instance_root = root("symlink-ancestor");
        let outside = root("symlink-outside");
        symlink(&outside, instance_root.join("config")).expect("symlink");

        let result = contained_path(&instance_root, "config/options.txt");

        assert!(matches!(result, Err(ContentError::Invalid(_))));
        assert!(!outside.join("options.txt").exists());
        let _ = fs::remove_dir_all(instance_root);
        let _ = fs::remove_dir_all(outside);
    }
}
