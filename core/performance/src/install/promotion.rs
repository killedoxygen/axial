use super::model::InstallError;
use crate::modrinth::ManagedDownloadTemp;
use std::path::{Path, PathBuf};

pub(super) async fn promote_file_async(
    mut temp: ManagedDownloadTemp,
    final_path: &Path,
    filename: &str,
    allow_overwrite: bool,
) -> Result<(), InstallError> {
    if allow_overwrite {
        let result = promote_file_with_overwrite_async(temp.path(), final_path).await;
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

    match tokio::fs::hard_link(temp.path(), final_path).await {
        Ok(()) => {
            if temp.cleanup().await.is_err() {
                tracing::warn!("promoted managed download temp cleanup remains pending");
            }
            Ok(())
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

pub(super) async fn promote_file_with_overwrite_async(
    temp_path: &Path,
    final_path: &Path,
) -> Result<(), InstallError> {
    let first_error = match tokio::fs::rename(temp_path, final_path).await {
        Ok(()) => return Ok(()),
        Err(error) => error,
    };

    match tokio::fs::symlink_metadata(temp_path).await {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(InstallError::Io(first_error));
        }
        Err(error) => return Err(InstallError::Io(error)),
    }

    let final_metadata = match tokio::fs::symlink_metadata(final_path).await {
        Ok(metadata) => Some(metadata),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(InstallError::Io(error)),
    };

    let Some(final_metadata) = final_metadata else {
        return match tokio::fs::rename(temp_path, final_path).await {
            Ok(()) => Ok(()),
            Err(error) => Err(InstallError::Io(error)),
        };
    };

    if final_metadata.is_dir() && !final_metadata.file_type().is_symlink() {
        return Err(InstallError::Io(first_error));
    }

    let backup_path = managed_artifact_replace_backup_path(final_path);
    if let Err(error) = tokio::fs::rename(final_path, &backup_path).await {
        return Err(InstallError::Io(error));
    }

    match tokio::fs::rename(temp_path, final_path).await {
        Ok(()) => {
            if tokio::fs::remove_file(&backup_path).await.is_err() {
                tracing::warn!("managed artifact replacement backup cleanup remains pending");
            }
            Ok(())
        }
        Err(error) => match tokio::fs::rename(&backup_path, final_path).await {
            Ok(()) => Err(InstallError::Io(error)),
            Err(restore_error) => Err(InstallError::Io(restore_error)),
        },
    }
}

fn managed_artifact_replace_backup_path(final_path: &Path) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|value| value.as_nanos())
        .unwrap_or_default();
    PathBuf::from(format!(
        "{}.replace-{}-{nanos:x}.tmp",
        final_path.display(),
        std::process::id()
    ))
}

pub(super) async fn reconcile_managed_replace_backups(
    final_path: &Path,
    ownership_proven: bool,
) -> Result<(), InstallError> {
    if !ownership_proven {
        return Ok(());
    }
    let Some(parent) = final_path.parent() else {
        return Ok(());
    };
    let Some(filename) = final_path.file_name().and_then(|value| value.to_str()) else {
        return Ok(());
    };
    let final_exists = match tokio::fs::symlink_metadata(final_path).await {
        Ok(metadata) if metadata.file_type().is_file() => true,
        Ok(_) => {
            return Err(InstallError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "managed artifact replacement target is not a regular file",
            )));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => return Err(InstallError::Io(error)),
    };
    let prefix = format!("{filename}.replace-");
    let mut backups = Vec::new();
    let mut entries = match tokio::fs::read_dir(parent).await {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(InstallError::Io(error)),
    };
    while let Some(entry) = entries.next_entry().await? {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !name.starts_with(&prefix) || !name.ends_with(".tmp") {
            continue;
        }
        let metadata = tokio::fs::symlink_metadata(entry.path()).await?;
        if !metadata.file_type().is_file() {
            return Err(InstallError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "managed artifact replacement backup is not a regular file",
            )));
        }
        backups.push(entry.path());
    }
    if final_exists {
        for backup in backups {
            tokio::fs::remove_file(backup).await?;
        }
    } else if backups.len() == 1 {
        tokio::fs::rename(&backups[0], final_path).await?;
    } else if !backups.is_empty() {
        return Err(InstallError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "managed artifact replacement has ambiguous recovery backups",
        )));
    }
    Ok(())
}
