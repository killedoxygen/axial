use super::model::InstallError;
use crate::modrinth::ManagedDownloadTemp;
use std::path::{Path, PathBuf};

const MUTATION_DIR_NAME: &str = "mutations";
const REPLACEMENT_DIR_NAME: &str = "replacements";
const MUTATION_ENTRY_LIMIT: usize = 256;

pub(super) async fn promote_file_async(
    mut temp: ManagedDownloadTemp,
    final_path: &Path,
    filename: &str,
    overwrite_expected_sha512: Option<&str>,
) -> Result<(), InstallError> {
    if let Some(expected_old_sha512) = overwrite_expected_sha512 {
        let result = promote_file_with_overwrite_async(
            temp.path(),
            final_path,
            expected_old_sha512,
            temp.sha512(),
        )
        .await;
        return match result {
            Ok(()) => {
                temp.disarm();
                Ok(())
            }
            Err(error) => match temp.cleanup().await {
                Ok(()) => Err(error),
                Err(cleanup_error) => Err(InstallError::Io(cleanup_error)),
            },
        };
    }

    let addition = stage_managed_addition_async(temp.path(), final_path, filename, temp.sha512())
        .await
        .map_err(InstallError::State)?;
    match tokio::fs::hard_link(&addition, final_path).await {
        Ok(()) => {
            let proof = async {
                let temp_metadata = tokio::fs::symlink_metadata(temp.path()).await?;
                let final_metadata = tokio::fs::symlink_metadata(final_path).await?;
                if !temp.owns_path_async(temp.path(), temp_metadata.len()).await
                    || !temp.owns_path_async(final_path, final_metadata.len()).await
                    || !super::artifact::file_matches_sha512(final_path, temp.sha512(), None)
                        .await?
                {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "managed artifact promotion identity cannot be proven",
                    ));
                }
                Ok::<(), std::io::Error>(())
            }
            .await;
            if let Err(error) = proof {
                if let Ok(metadata) = tokio::fs::symlink_metadata(final_path).await
                    && temp.owns_path_async(final_path, metadata.len()).await
                {
                    tokio::fs::remove_file(final_path).await?;
                }
                return match temp.cleanup().await {
                    Ok(()) => Err(InstallError::Io(error)),
                    Err(cleanup) => Err(InstallError::Io(cleanup)),
                };
            }
            temp.cleanup().await.map_err(InstallError::Io)
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            match temp.cleanup().await {
                Ok(()) => Err(InstallError::ManagedArtifactTargetExists(
                    filename.to_string(),
                )),
                Err(cleanup_error) => Err(InstallError::Io(cleanup_error)),
            }
        }
        Err(error) => match temp.cleanup().await {
            Ok(()) => Err(InstallError::Io(error)),
            Err(cleanup_error) => Err(InstallError::Io(cleanup_error)),
        },
    }
}

async fn stage_managed_addition_async(
    temp_path: &Path,
    final_path: &Path,
    filename: &str,
    sha512: &str,
) -> Result<PathBuf, crate::state::StateError> {
    let instance_mods_dir = final_path
        .parent()
        .ok_or_else(|| crate::state::StateError::InvalidFilename(filename.to_string()))?
        .to_path_buf();
    let temp_path = temp_path.to_path_buf();
    let filename = filename.to_string();
    let sha512 = sha512.to_string();
    tokio::task::spawn_blocking(move || {
        crate::state::stage_managed_artifact_addition(
            &instance_mods_dir,
            &filename,
            &sha512,
            &temp_path,
        )
    })
    .await
    .map_err(|_| {
        crate::state::StateError::Read(std::io::Error::other(
            "managed addition staging task stopped",
        ))
    })?
}

pub(super) async fn promote_file_with_overwrite_async(
    temp_path: &Path,
    final_path: &Path,
    expected_old_sha512: &str,
    expected_new_sha512: &str,
) -> Result<(), InstallError> {
    if !super::artifact::file_matches_sha512(final_path, expected_old_sha512, None).await? {
        return Err(target_exists(final_path));
    }
    if !super::artifact::file_matches_sha512(temp_path, expected_new_sha512, None).await? {
        return Err(InstallError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "managed artifact download digest changed before promotion",
        )));
    }

    let backup_path =
        managed_artifact_replace_backup_path(final_path, expected_old_sha512, expected_new_sha512)
            .await?;
    tokio::fs::hard_link(final_path, &backup_path).await?;
    let source = crate::file_identity::admit_async(final_path).await?;
    let source_identity = source.identity();
    let source_len = source.metadata().len();
    drop(source);
    if crate::file_identity::revalidate_async(&backup_path, source_identity, source_len)
        .await
        .is_err()
        || !super::artifact::file_matches_sha512(&backup_path, expected_old_sha512, None).await?
    {
        remove_if_identity(&backup_path, source_identity, source_len).await?;
        return Err(target_exists(final_path));
    }
    if crate::file_identity::revalidate_async(final_path, source_identity, source_len)
        .await
        .is_err()
    {
        remove_if_identity(&backup_path, source_identity, source_len).await?;
        return Err(target_exists(final_path));
    }
    remove_if_identity(final_path, source_identity, source_len).await?;
    match tokio::fs::rename(temp_path, final_path).await {
        Ok(()) => Ok(()),
        Err(error) => match tokio::fs::rename(&backup_path, final_path).await {
            Ok(()) => Err(InstallError::Io(error)),
            Err(restore_error) => Err(InstallError::Io(restore_error)),
        },
    }
}

pub(super) async fn settle_managed_replace_backup(
    final_path: &Path,
    expected_old_sha512: &str,
    expected_new_sha512: &str,
) -> Result<(), InstallError> {
    let backup =
        managed_artifact_replace_backup_path(final_path, expected_old_sha512, expected_new_sha512)
            .await?;
    if !super::artifact::file_matches_sha512(final_path, expected_new_sha512, None).await?
        || !super::artifact::file_matches_sha512(&backup, expected_old_sha512, None).await?
    {
        return Err(InstallError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "managed artifact replacement settlement ownership cannot be proven",
        )));
    }
    remove_digest_proven(&backup, expected_old_sha512).await?;
    Ok(())
}

async fn managed_artifact_replace_backup_path(
    final_path: &Path,
    old_sha512: &str,
    new_sha512: &str,
) -> Result<PathBuf, InstallError> {
    if !valid_digest(old_sha512) || !valid_digest(new_sha512) {
        return Err(invalid_mutation_root());
    }
    let parent = final_path.parent().ok_or_else(invalid_mutation_root)?;
    let filename = final_path
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(invalid_mutation_root)?;
    let replacement_root = parent
        .join(crate::state::STATE_DIR_NAME)
        .join(MUTATION_DIR_NAME)
        .join(REPLACEMENT_DIR_NAME);
    let old_dir = replacement_root.join(old_sha512);
    let new_dir = old_dir.join(new_sha512);
    for directory in [
        parent.join(crate::state::STATE_DIR_NAME),
        parent
            .join(crate::state::STATE_DIR_NAME)
            .join(MUTATION_DIR_NAME),
        replacement_root,
        old_dir,
        new_dir.clone(),
    ] {
        ensure_mutation_directory(&directory).await?;
    }
    Ok(new_dir.join(filename))
}

pub(super) async fn reconcile_managed_replace_backups(
    final_path: &Path,
    expected_old_sha512: Option<&str>,
) -> Result<(), InstallError> {
    let Some(expected_old_sha512) = expected_old_sha512 else {
        return Ok(());
    };
    let Some(parent) = final_path.parent() else {
        return Ok(());
    };
    let Some(filename) = final_path.file_name().and_then(|value| value.to_str()) else {
        return Ok(());
    };
    let final_exists = match tokio::fs::symlink_metadata(final_path).await {
        Ok(metadata) if metadata.file_type().is_file() => true,
        Ok(_) => return Err(target_exists(final_path)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => return Err(InstallError::Io(error)),
    };
    let mut backups = Vec::new();
    let mut entry_count = 0_usize;
    let replacements = parent
        .join(crate::state::STATE_DIR_NAME)
        .join(MUTATION_DIR_NAME)
        .join(REPLACEMENT_DIR_NAME);
    validate_replacement_roots(parent).await?;
    let mut old_entries = match tokio::fs::read_dir(&replacements).await {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(InstallError::Io(error)),
    };
    while let Some(old_entry) = old_entries.next_entry().await? {
        entry_count += 1;
        if entry_count > MUTATION_ENTRY_LIMIT {
            return Err(InstallError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "managed artifact replacement obligations exceed the limit",
            )));
        }
        let Some(old_sha512) = digest_directory_name(&old_entry).await? else {
            return Err(invalid_mutation_root());
        };
        let mut new_entries = tokio::fs::read_dir(old_entry.path()).await?;
        while let Some(new_entry) = new_entries.next_entry().await? {
            entry_count += 1;
            if entry_count > MUTATION_ENTRY_LIMIT {
                return Err(InstallError::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "managed artifact replacement obligations exceed the limit",
                )));
            }
            let Some(new_sha512) = digest_directory_name(&new_entry).await? else {
                return Err(invalid_mutation_root());
            };
            let backup = new_entry.path().join(filename);
            match tokio::fs::symlink_metadata(&backup).await {
                Ok(metadata) if metadata.file_type().is_file() => {
                    backups.push((backup, old_sha512.clone(), new_sha512))
                }
                Ok(_) => return Err(invalid_mutation_root()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(InstallError::Io(error)),
            }
        }
    }
    if backups.len() > 1 {
        return Err(InstallError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "managed artifact replacement has ambiguous recovery backups",
        )));
    }
    let Some((backup, backup_old_sha512, new_sha512)) = backups.pop() else {
        if final_exists
            && !super::artifact::file_matches_sha512(final_path, expected_old_sha512, None).await?
        {
            return Err(target_exists(final_path));
        }
        return Ok(());
    };
    if !super::artifact::file_matches_sha512(&backup, &backup_old_sha512, None).await? {
        return Err(InstallError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "managed artifact replacement backup ownership cannot be proven",
        )));
    }
    if !final_exists && !expected_old_sha512.eq_ignore_ascii_case(&backup_old_sha512) {
        return Err(target_exists(final_path));
    }
    if final_exists {
        let final_matches_old =
            super::artifact::file_matches_sha512(final_path, &backup_old_sha512, None).await?;
        let final_matches_new =
            super::artifact::file_matches_sha512(final_path, &new_sha512, None).await?;
        if final_matches_old {
            remove_digest_proven(&backup, &backup_old_sha512).await?;
            return Ok(());
        }
        if !final_matches_new {
            return Err(target_exists(final_path));
        }
        if expected_old_sha512.eq_ignore_ascii_case(&new_sha512) {
            remove_digest_proven(&backup, &backup_old_sha512).await?;
            return Ok(());
        }
        if !expected_old_sha512.eq_ignore_ascii_case(&backup_old_sha512) {
            return Err(target_exists(final_path));
        }
        remove_digest_proven(final_path, &new_sha512).await?;
    }
    tokio::fs::rename(backup, final_path).await?;
    Ok(())
}

pub(super) async fn settle_empty_managed_replace_root(
    instance_mods_dir: &Path,
) -> Result<(), InstallError> {
    let replacements = instance_mods_dir
        .join(crate::state::STATE_DIR_NAME)
        .join(MUTATION_DIR_NAME)
        .join(REPLACEMENT_DIR_NAME);
    validate_replacement_roots(instance_mods_dir).await?;
    let mut old_entries = match tokio::fs::read_dir(&replacements).await {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(InstallError::Io(error)),
    };
    let mut old_dirs = Vec::new();
    let mut entry_count = 0_usize;
    while let Some(old_entry) = old_entries.next_entry().await? {
        entry_count += 1;
        if entry_count > MUTATION_ENTRY_LIMIT || digest_directory_name(&old_entry).await?.is_none()
        {
            return Err(invalid_mutation_root());
        }
        let old_path = old_entry.path();
        let mut new_entries = tokio::fs::read_dir(&old_path).await?;
        let mut new_dirs = Vec::new();
        while let Some(new_entry) = new_entries.next_entry().await? {
            entry_count += 1;
            if entry_count > MUTATION_ENTRY_LIMIT
                || digest_directory_name(&new_entry).await?.is_none()
            {
                return Err(invalid_mutation_root());
            }
            let new_path = new_entry.path();
            if tokio::fs::read_dir(&new_path)
                .await?
                .next_entry()
                .await?
                .is_some()
            {
                return Err(invalid_mutation_root());
            }
            new_dirs.push(new_path);
        }
        for new_dir in new_dirs {
            tokio::fs::remove_dir(new_dir).await?;
        }
        old_dirs.push(old_path);
    }
    for old_dir in old_dirs {
        tokio::fs::remove_dir(old_dir).await?;
    }
    tokio::fs::remove_dir(replacements).await?;
    Ok(())
}

async fn remove_digest_proven(path: &Path, digest: &str) -> Result<(), InstallError> {
    let admitted = crate::file_identity::admit_async(path).await?;
    let identity = admitted.identity();
    let len = admitted.metadata().len();
    drop(admitted);
    if !super::artifact::file_matches_sha512(path, digest, None).await?
        || crate::file_identity::revalidate_async(path, identity, len)
            .await
            .is_err()
    {
        return Err(invalid_mutation_root());
    }
    remove_if_identity(path, identity, len).await
}

async fn remove_if_identity(
    path: &Path,
    admitted: crate::file_identity::FileIdentity,
    admitted_len: u64,
) -> Result<(), InstallError> {
    if crate::file_identity::revalidate_async(path, admitted, admitted_len)
        .await
        .is_err()
    {
        return Err(invalid_mutation_root());
    }
    tokio::fs::remove_file(path).await?;
    Ok(())
}

async fn ensure_mutation_directory(path: &Path) -> Result<(), InstallError> {
    match tokio::fs::symlink_metadata(path).await {
        Ok(metadata) if metadata.file_type().is_dir() => Ok(()),
        Ok(_) => Err(invalid_mutation_root()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            tokio::fs::create_dir(path).await?;
            let metadata = tokio::fs::symlink_metadata(path).await?;
            if metadata.file_type().is_dir() {
                Ok(())
            } else {
                Err(invalid_mutation_root())
            }
        }
        Err(error) => Err(InstallError::Io(error)),
    }
}

async fn validate_replacement_roots(instance_mods_dir: &Path) -> Result<(), InstallError> {
    for path in [
        instance_mods_dir.join(crate::state::STATE_DIR_NAME),
        instance_mods_dir
            .join(crate::state::STATE_DIR_NAME)
            .join(MUTATION_DIR_NAME),
        instance_mods_dir
            .join(crate::state::STATE_DIR_NAME)
            .join(MUTATION_DIR_NAME)
            .join(REPLACEMENT_DIR_NAME),
    ] {
        match tokio::fs::symlink_metadata(path).await {
            Ok(metadata) if metadata.file_type().is_dir() => {}
            Ok(_) => return Err(invalid_mutation_root()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(InstallError::Io(error)),
        }
    }
    Ok(())
}

async fn digest_directory_name(
    entry: &tokio::fs::DirEntry,
) -> Result<Option<String>, InstallError> {
    if !entry.file_type().await?.is_dir() {
        return Ok(None);
    }
    let name = entry.file_name();
    let Some(name) = name.to_str().map(str::to_string) else {
        return Ok(None);
    };
    Ok((name.len() == 128 && name.bytes().all(|byte| byte.is_ascii_hexdigit())).then_some(name))
}

fn valid_digest(value: &str) -> bool {
    value.len() == 128 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn invalid_mutation_root() -> InstallError {
    InstallError::Io(std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        "managed artifact mutation obligation is invalid",
    ))
}

fn target_exists(path: &Path) -> InstallError {
    InstallError::ManagedArtifactTargetExists(
        path.file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("managed artifact")
            .to_string(),
    )
}
