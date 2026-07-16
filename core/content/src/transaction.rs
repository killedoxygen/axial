use crate::error::{ContentError, ContentResult};
use std::collections::HashSet;
use std::fs;
use std::io::{self, Read};
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
    applied: Vec<AppliedFile>,
    removed: Vec<String>,
    replace_existing: bool,
    must_be_absent: HashSet<String>,
    preserve_staging: bool,
    finished: bool,
}

#[derive(Debug, Clone)]
struct AppliedFile {
    relative: String,
    existed: bool,
    expected: PathBuf,
}

impl FileTransaction {
    pub(crate) fn apply(
        root: &Path,
        staging: PathBuf,
        relative_paths: &[String],
    ) -> ContentResult<Self> {
        Self::apply_with_policy(
            root,
            staging,
            relative_paths,
            true,
            &[],
            &mut allow_existing_destination,
        )
    }

    /// Apply replacements by claiming each existing destination into the unique
    /// transaction backup, validating those claimed bytes, and promoting with
    /// no-clobber semantics.
    pub(crate) fn apply_preserving_absence_with_revalidation<F>(
        root: &Path,
        staging: PathBuf,
        relative_paths: &[String],
        must_be_absent: &[String],
        mut validate_existing: F,
    ) -> ContentResult<Self>
    where
        F: FnMut(&str, &Path) -> ContentResult<()>,
    {
        Self::apply_with_policy(
            root,
            staging,
            relative_paths,
            true,
            must_be_absent,
            &mut validate_existing,
        )
    }

    pub(crate) fn apply_new(
        root: &Path,
        staging: PathBuf,
        relative_paths: &[String],
    ) -> ContentResult<Self> {
        Self::apply_with_policy(
            root,
            staging,
            relative_paths,
            false,
            relative_paths,
            &mut allow_existing_destination,
        )
    }

    fn apply_with_policy<F>(
        root: &Path,
        staging: PathBuf,
        relative_paths: &[String],
        replace_existing: bool,
        must_be_absent: &[String],
        validate_existing: &mut F,
    ) -> ContentResult<Self>
    where
        F: FnMut(&str, &Path) -> ContentResult<()>,
    {
        let backup = staging.join(".backup");
        let mut transaction = Self {
            root: root.to_path_buf(),
            staging,
            backup,
            applied: Vec::new(),
            removed: Vec::new(),
            replace_existing,
            must_be_absent: must_be_absent.iter().cloned().collect(),
            preserve_staging: false,
            finished: false,
        };
        for relative in relative_paths {
            if let Err(error) = transaction.apply_one(relative, validate_existing) {
                if let Err(rollback_error) = transaction.rollback_inner() {
                    transaction.finished = true;
                    return Err(rollback_error);
                }
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

    /// Claim existing destinations into the transaction backup and validate the
    /// claimed bytes before removal can become part of the transaction. If a
    /// later claim fails, earlier removals remain staged and are restored by an
    /// explicit rollback or by dropping the transaction.
    pub(crate) fn stage_removals_with_revalidation<F>(
        &mut self,
        relative_paths: &[String],
        mut validate_claimed: F,
    ) -> ContentResult<()>
    where
        F: FnMut(&str, &Path) -> ContentResult<()>,
    {
        for relative in relative_paths {
            self.stage_removal(relative, &mut validate_claimed)?;
        }
        Ok(())
    }

    /// Atomically claim `source`, classify the claimed bytes, and publish an
    /// identical file at an absent `target`. Rollback compares the published
    /// bytes with the retained claim before removing them and restores the
    /// source without replacing a path that appeared in the meantime.
    pub(crate) fn move_new_with_revalidation<T, F, P>(
        &mut self,
        source: &str,
        target: &str,
        validate_claimed: F,
        before_publish: P,
    ) -> ContentResult<T>
    where
        F: FnOnce(&Path) -> ContentResult<T>,
        P: FnOnce(),
    {
        if source == target
            || self
                .applied
                .iter()
                .any(|applied| applied.relative == source || applied.relative == target)
            || self
                .removed
                .iter()
                .any(|removed| removed == source || removed == target)
        {
            return Err(ContentError::Invalid(
                "content move overlaps another transaction path".to_string(),
            ));
        }

        let source_path = contained_path(&self.root, source)?;
        let target_path = contained_path(&self.root, target)?;
        match fs::symlink_metadata(&source_path) {
            Ok(metadata) if metadata.is_file() => {}
            Ok(_) => {
                return Err(ContentError::Invalid(
                    "content move source is not a regular file".to_string(),
                ));
            }
            Err(error) => return Err(ContentError::Io(error)),
        }
        match fs::symlink_metadata(&target_path) {
            Ok(_) => {
                return Err(ContentError::Invalid(
                    "content destination became occupied before commit".to_string(),
                ));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(ContentError::Io(error)),
        }

        let claimed = contained_path(&self.backup, source)?;
        if let Some(parent) = claimed.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::rename(&source_path, &claimed)?;
        let validation = match validate_claimed(&claimed) {
            Ok(validation) => validation,
            Err(error) => {
                return Err(self.restore_claimed_or_retain(&claimed, &source_path, error));
            }
        };

        before_publish();
        let publish_result = promote_new_file_retaining_source(&claimed, &target_path);
        if let Err(error) = publish_result {
            return Err(self.restore_claimed_or_retain(&claimed, &source_path, error));
        }
        self.removed.push(source.to_string());
        self.applied.push(AppliedFile {
            relative: target.to_string(),
            existed: false,
            expected: claimed,
        });
        Ok(validation)
    }

    fn stage_removal<F>(&mut self, relative: &str, validate_claimed: &mut F) -> ContentResult<()>
    where
        F: FnMut(&str, &Path) -> ContentResult<()>,
    {
        if self
            .applied
            .iter()
            .any(|applied| applied.relative == relative)
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
        if let Err(error) = validate_claimed(relative, &backup) {
            return Err(self.restore_claimed_or_retain(&backup, &destination, error));
        }
        self.removed.push(relative.to_string());
        Ok(())
    }

    fn apply_one<F>(&mut self, relative: &str, validate_existing: &mut F) -> ContentResult<()>
    where
        F: FnMut(&str, &Path) -> ContentResult<()>,
    {
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
        if existed && (!self.replace_existing || self.must_be_absent.contains(relative)) {
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
            if let Err(error) = validate_existing(relative, &backup) {
                return Err(self.restore_claimed_or_retain(&backup, &destination, error));
            }
        }
        let promote_result = promote_new_file_retaining_source(&staged, &destination);
        if let Err(error) = promote_result {
            if existed {
                return Err(self.restore_claimed_or_retain(&backup, &destination, error));
            }
            return Err(error);
        }
        self.applied.push(AppliedFile {
            relative: relative.to_string(),
            existed,
            expected: staged,
        });
        Ok(())
    }

    fn restore_claimed_or_retain(
        &mut self,
        backup: &Path,
        destination: &Path,
        original_error: ContentError,
    ) -> ContentError {
        match promote_new_file(backup, destination) {
            Ok(()) => original_error,
            Err(_) => {
                self.preserve_staging = true;
                ContentError::Invalid(
                    "content changed before commit and recovery bytes were retained because the destination became occupied"
                        .to_string(),
                )
            }
        }
    }

    pub(crate) fn commit(mut self) {
        self.finished = true;
        if !self.preserve_staging {
            let _ = fs::remove_dir_all(&self.staging);
        }
    }

    pub(crate) fn rollback(mut self) -> ContentResult<()> {
        let result = self.rollback_inner();
        self.finished = true;
        result
    }

    fn rollback_inner(&mut self) -> ContentResult<()> {
        let mut failed = false;
        let applied = self.applied.clone();
        for applied in applied.iter().rev() {
            if self.rollback_applied(applied).is_err() {
                self.preserve_staging = true;
                failed = true;
            }
        }
        let removed = self.removed.clone();
        for relative in removed.iter().rev() {
            if let (Ok(destination), Ok(backup)) = (
                contained_path(&self.root, relative),
                contained_path(&self.backup, relative),
            ) && restore_without_clobber(&backup, &destination).is_err()
            {
                self.preserve_staging = true;
                failed = true;
            }
        }
        if !self.preserve_staging {
            let _ = fs::remove_dir_all(&self.staging);
        }
        if failed {
            Err(ContentError::Invalid(
                "content rollback could not restore every path without replacing newer filesystem changes; recovery bytes were retained"
                    .to_string(),
            ))
        } else {
            Ok(())
        }
    }

    fn rollback_applied(&mut self, applied: &AppliedFile) -> ContentResult<()> {
        let destination = contained_path(&self.root, &applied.relative)?;
        let backup = contained_path(&self.backup, &applied.relative)?;
        let rollback_claim =
            contained_path(&self.staging.join(".rollback-current"), &applied.relative)?;
        let current_exists = match fs::symlink_metadata(&destination) {
            Ok(_) => true,
            Err(error) if error.kind() == io::ErrorKind::NotFound => false,
            Err(error) => return Err(ContentError::Io(error)),
        };

        if !current_exists {
            if applied.existed {
                return Err(ContentError::Invalid(
                    "an applied content destination changed before rollback".to_string(),
                ));
            }
            return Ok(());
        }
        if let Some(parent) = rollback_claim.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::rename(&destination, &rollback_claim)?;
        if !regular_files_match(&rollback_claim, &applied.expected)? {
            self.preserve_staging = true;
            restore_without_clobber(&rollback_claim, &destination)?;
            return Err(ContentError::Invalid(
                "an applied content destination changed before rollback".to_string(),
            ));
        }
        fs::remove_file(&rollback_claim)?;
        if applied.existed {
            restore_without_clobber(&backup, &destination)?;
        }
        Ok(())
    }
}

fn allow_existing_destination(_: &str, _: &Path) -> ContentResult<()> {
    Ok(())
}

fn restore_without_clobber(claimed: &Path, destination: &Path) -> ContentResult<()> {
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
    }
    promote_new_file(claimed, destination)
}

fn regular_files_match(left: &Path, right: &Path) -> ContentResult<bool> {
    let left_metadata = fs::symlink_metadata(left)?;
    let right_metadata = fs::symlink_metadata(right)?;
    if !left_metadata.is_file()
        || !right_metadata.is_file()
        || left_metadata.len() != right_metadata.len()
    {
        return Ok(false);
    }
    let mut left = fs::File::open(left)?;
    let mut right = fs::File::open(right)?;
    let mut left_buffer = [0_u8; 64 * 1024];
    let mut right_buffer = [0_u8; 64 * 1024];
    loop {
        let left_read = left.read(&mut left_buffer)?;
        let right_read = right.read(&mut right_buffer)?;
        if left_read != right_read || left_buffer[..left_read] != right_buffer[..right_read] {
            return Ok(false);
        }
        if left_read == 0 {
            return Ok(true);
        }
    }
}

/// Promote a staged regular file without ever replacing an occupied path. A
/// hard link provides an atomic same-volume fast path. The fallback first
/// copies into a unique private directory beside the destination, then
/// publishes the completed copy atomically without replacing an occupied path.
fn promote_new_file(staged: &Path, destination: &Path) -> ContentResult<()> {
    promote_new_file_with_source_policy(staged, destination, true)
}

fn promote_new_file_retaining_source(staged: &Path, destination: &Path) -> ContentResult<()> {
    promote_new_file_with_source_policy(staged, destination, false)
}

fn promote_new_file_with_source_policy(
    staged: &Path,
    destination: &Path,
    remove_source: bool,
) -> ContentResult<()> {
    promote_new_file_with_copy(staged, destination, remove_source, |source, destination| {
        fs::copy(source, destination)
    })
}

fn promote_new_file_with_copy<F>(
    staged: &Path,
    destination: &Path,
    remove_source: bool,
    copy_file: F,
) -> ContentResult<()>
where
    F: FnOnce(&Path, &Path) -> io::Result<u64>,
{
    if remove_source {
        match fs::hard_link(staged, destination) {
            Ok(()) => {
                let _ = fs::remove_file(staged);
                return Ok(());
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                return Err(ContentError::Invalid(
                    "content destination became occupied before commit".to_string(),
                ));
            }
            Err(_) => {}
        }
    }

    let parent = destination.parent().unwrap_or_else(|| Path::new("."));
    let copy_root = staging_dir(parent, "axial-content-promotion");
    fs::create_dir(&copy_root)?;
    let private_copy = copy_root.join("payload");
    if let Err(error) = copy_file(staged, &private_copy) {
        let _ = fs::remove_dir_all(&copy_root);
        return Err(ContentError::Io(error));
    }
    let publish_result = publish_private_copy(&private_copy, destination);
    let _ = fs::remove_dir_all(&copy_root);
    publish_result?;
    if remove_source {
        let _ = fs::remove_file(staged);
    }
    Ok(())
}

fn publish_private_copy(private_copy: &Path, destination: &Path) -> ContentResult<()> {
    match fs::hard_link(private_copy, destination) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Err(ContentError::Invalid(
            "content destination became occupied before commit".to_string(),
        )),
        Err(error) => Err(ContentError::Io(error)),
    }
}

impl Drop for FileTransaction {
    fn drop(&mut self) {
        if !self.finished {
            let _ = self.rollback_inner();
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
            .rollback()
            .expect("rollback");

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
            .stage_removals_with_revalidation(&["mods/example.jar".to_string()], |_, _| Ok(()))
            .expect("stage removal");
        assert!(!root.join("mods/example.jar").exists());
        transaction.rollback().expect("rollback");

        assert_eq!(
            fs::read(root.join("mods/example.jar")).expect("restored"),
            b"content"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn partial_removal_staging_is_restored_by_caller_rollback() {
        let root = root("partial-removal-staging");
        fs::create_dir_all(root.join("mods")).expect("mods");
        let first = root.join("mods/first.jar");
        let second = root.join("mods/second.jar");
        fs::write(&first, b"first").expect("first");
        fs::write(&second, b"second").expect("second");
        let mut transaction = FileTransaction::empty(&root).expect("transaction");

        let result = transaction.stage_removals_with_revalidation(
            &["mods/first.jar".to_string(), "mods/second.jar".to_string()],
            |relative, _| {
                if relative == "mods/second.jar" {
                    Err(ContentError::Invalid("reject second removal".to_string()))
                } else {
                    Ok(())
                }
            },
        );

        assert!(result.is_err());
        assert!(!first.exists());
        assert_eq!(fs::read(&second).expect("restored second"), b"second");
        transaction.rollback().expect("caller rollback");
        assert_eq!(fs::read(&first).expect("restored first"), b"first");
        assert_eq!(fs::read(&second).expect("preserved second"), b"second");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn removal_rollback_preserves_a_new_destination_and_retains_the_backup() {
        let root = root("remove-rollback-conflict");
        fs::create_dir_all(root.join("mods")).expect("mods");
        let destination = root.join("mods/example.jar");
        fs::write(&destination, b"removed bytes").expect("removal source");
        let staging = StagingGuard::create(&root, "stage").expect("stage");
        let staging_root = staging.path().to_path_buf();
        let mut transaction =
            FileTransaction::apply(&root, staging.transfer(), &[]).expect("transaction");
        transaction
            .stage_removals_with_revalidation(&["mods/example.jar".to_string()], |_, _| Ok(()))
            .expect("stage removal");

        fs::write(&destination, b"new destination").expect("racing destination");
        let error = transaction
            .rollback()
            .expect_err("rollback must not replace a new destination");

        assert!(error.to_string().contains("rollback"));
        assert_eq!(
            fs::read(&destination).expect("preserved new destination"),
            b"new destination"
        );
        assert_eq!(
            fs::read(staging_root.join(".backup/mods/example.jar"))
                .expect("retained removal backup"),
            b"removed bytes"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn partial_apply_rollback_preserves_a_replaced_earlier_destination() {
        let root = root("partial-rollback-user-replacement");
        fs::create_dir_all(root.join("mods")).expect("mods");
        let first = root.join("mods/first.jar");
        let second = root.join("mods/second.jar");
        fs::write(&first, b"old first").expect("old first");
        fs::write(&second, b"old second").expect("old second");
        let staging = StagingGuard::create(&root, "stage").expect("stage");
        let staging_root = staging.path().to_path_buf();
        fs::create_dir_all(staging.path().join("mods")).expect("staged mods");
        fs::write(staging.path().join("mods/first.jar"), b"new first").expect("new first");
        fs::write(staging.path().join("mods/second.jar"), b"new second").expect("new second");

        let result = FileTransaction::apply_preserving_absence_with_revalidation(
            &root,
            staging.transfer(),
            &["mods/first.jar".to_string(), "mods/second.jar".to_string()],
            &[],
            |relative, _| {
                if relative == "mods/second.jar" {
                    fs::write(&first, b"user replacement").expect("replace first after apply");
                    return Err(ContentError::Invalid(
                        "second destination failed validation".to_string(),
                    ));
                }
                Ok(())
            },
        );

        assert!(result.is_err());
        assert_eq!(
            fs::read(&first).expect("preserved first replacement"),
            b"user replacement"
        );
        assert_eq!(fs::read(&second).expect("restored second"), b"old second");
        assert_eq!(
            fs::read(staging_root.join(".backup/mods/first.jar")).expect("retained first backup"),
            b"old first"
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
            .stage_removals_with_revalidation(&["mods/example.jar".to_string()], |_, _| Ok(()))
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
    fn replacement_transaction_preserves_destinations_that_preflight_found_absent() {
        let root = root("preflight-absence");
        fs::create_dir_all(root.join("mods")).expect("mods");
        fs::write(root.join("mods/existing.jar"), b"managed old").expect("managed old");
        let staging = StagingGuard::create(&root, "stage").expect("stage");
        fs::create_dir_all(staging.path().join("mods")).expect("staged mods");
        fs::write(staging.path().join("mods/existing.jar"), b"managed new")
            .expect("managed replacement");
        fs::write(staging.path().join("mods/new.jar"), b"downloaded").expect("new download");

        // Simulate a user adding the destination after preflight but before
        // the downloaded batch is committed.
        fs::write(root.join("mods/new.jar"), b"user file").expect("racing user file");
        let paths = vec!["mods/existing.jar".to_string(), "mods/new.jar".to_string()];
        let result = FileTransaction::apply_preserving_absence_with_revalidation(
            &root,
            staging.transfer(),
            &paths,
            &["mods/new.jar".to_string()],
            |_, _| Ok(()),
        );

        assert!(result.is_err());
        assert_eq!(
            fs::read(root.join("mods/existing.jar")).expect("restored managed file"),
            b"managed old"
        );
        assert_eq!(
            fs::read(root.join("mods/new.jar")).expect("preserved user file"),
            b"user file"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn failed_claim_validation_preserves_a_new_destination_and_retains_the_claimed_file() {
        let root = root("claim-validation-conflict");
        fs::create_dir_all(root.join("mods")).expect("mods");
        let destination = root.join("mods/example.jar");
        fs::write(&destination, b"claimed bytes").expect("existing file");
        let staging = StagingGuard::create(&root, "stage").expect("stage");
        let staging_root = staging.path().to_path_buf();
        fs::create_dir_all(staging.path().join("mods")).expect("staged mods");
        fs::write(
            staging.path().join("mods/example.jar"),
            b"downloaded update",
        )
        .expect("staged update");

        let result = FileTransaction::apply_preserving_absence_with_revalidation(
            &root,
            staging.transfer(),
            &["mods/example.jar".to_string()],
            &[],
            |_, claimed| {
                assert_eq!(fs::read(claimed).expect("claimed file"), b"claimed bytes");
                fs::write(&destination, b"new destination").expect("racing destination");
                Err(ContentError::Invalid(
                    "claimed destination failed validation".to_string(),
                ))
            },
        );

        assert!(result.is_err());
        assert_eq!(
            fs::read(&destination).expect("preserved new destination"),
            b"new destination"
        );
        assert_eq!(
            fs::read(staging_root.join(".backup/mods/example.jar")).expect("retained claimed file"),
            b"claimed bytes"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn failed_private_copy_does_not_remove_a_new_destination() {
        let root = root("failed-private-copy");
        let source = root.join("source.jar");
        let destination = root.join("destination.jar");
        fs::write(&source, b"managed bytes").expect("source");

        let result = promote_new_file_with_copy(&source, &destination, false, |_, private_copy| {
            fs::write(private_copy, b"partial private copy")?;
            fs::write(&destination, b"user replacement")?;
            Err(io::Error::other("simulated copy failure"))
        });

        assert!(result.is_err());
        assert_eq!(
            fs::read(&destination).expect("preserved replacement"),
            b"user replacement"
        );
        assert_eq!(
            fs::read(&source).expect("preserved source"),
            b"managed bytes"
        );
        let promotion_directories = fs::read_dir(&root)
            .expect("root entries")
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".axial-content-promotion-")
            })
            .count();
        assert_eq!(promotion_directories, 0);
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
