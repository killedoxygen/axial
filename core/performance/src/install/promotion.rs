use super::model::InstallError;
#[cfg(test)]
use std::fs;
use std::path::{Path, PathBuf};

pub(super) async fn promote_file_async(
    temp_path: &Path,
    final_path: &Path,
    filename: &str,
    allow_overwrite: bool,
) -> Result<(), InstallError> {
    if allow_overwrite {
        return promote_file_with_overwrite_async(temp_path, final_path).await;
    }

    match tokio::fs::hard_link(temp_path, final_path).await {
        Ok(()) => {
            let _ = tokio::fs::remove_file(temp_path).await;
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let _ = tokio::fs::remove_file(temp_path).await;
            Err(InstallError::ManagedArtifactTargetExists(
                filename.to_string(),
            ))
        }
        Err(error) => {
            let _ = tokio::fs::remove_file(temp_path).await;
            Err(InstallError::Io(error))
        }
    }
}

#[cfg(test)]
pub(super) fn promote_file_with_overwrite(
    temp_path: &Path,
    final_path: &Path,
) -> Result<(), InstallError> {
    let first_error = match fs::rename(temp_path, final_path) {
        Ok(()) => return Ok(()),
        Err(error) => error,
    };

    match fs::symlink_metadata(temp_path) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(InstallError::Io(first_error));
        }
        Err(error) => return Err(InstallError::Io(error)),
    }

    let final_metadata = match fs::symlink_metadata(final_path) {
        Ok(metadata) => Some(metadata),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            let _ = fs::remove_file(temp_path);
            return Err(InstallError::Io(error));
        }
    };

    let Some(final_metadata) = final_metadata else {
        return match fs::rename(temp_path, final_path) {
            Ok(()) => Ok(()),
            Err(error) => {
                let _ = fs::remove_file(temp_path);
                Err(InstallError::Io(error))
            }
        };
    };

    if final_metadata.is_dir() && !final_metadata.file_type().is_symlink() {
        let _ = fs::remove_file(temp_path);
        return Err(InstallError::Io(first_error));
    }

    let backup_path = managed_artifact_replace_backup_path(final_path);
    fs::rename(final_path, &backup_path).map_err(|error| {
        let _ = fs::remove_file(temp_path);
        InstallError::Io(error)
    })?;

    match fs::rename(temp_path, final_path) {
        Ok(()) => {
            let _ = fs::remove_file(&backup_path);
            Ok(())
        }
        Err(error) => {
            let _ = fs::rename(&backup_path, final_path);
            let _ = fs::remove_file(temp_path);
            Err(InstallError::Io(error))
        }
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
        Err(error) => {
            let _ = tokio::fs::remove_file(temp_path).await;
            return Err(InstallError::Io(error));
        }
    };

    let Some(final_metadata) = final_metadata else {
        return match tokio::fs::rename(temp_path, final_path).await {
            Ok(()) => Ok(()),
            Err(error) => {
                let _ = tokio::fs::remove_file(temp_path).await;
                Err(InstallError::Io(error))
            }
        };
    };

    if final_metadata.is_dir() && !final_metadata.file_type().is_symlink() {
        let _ = tokio::fs::remove_file(temp_path).await;
        return Err(InstallError::Io(first_error));
    }

    let backup_path = managed_artifact_replace_backup_path(final_path);
    if let Err(error) = tokio::fs::rename(final_path, &backup_path).await {
        let _ = tokio::fs::remove_file(temp_path).await;
        return Err(InstallError::Io(error));
    }

    match tokio::fs::rename(temp_path, final_path).await {
        Ok(()) => {
            let _ = tokio::fs::remove_file(&backup_path).await;
            Ok(())
        }
        Err(error) => {
            let _ = tokio::fs::rename(&backup_path, final_path).await;
            let _ = tokio::fs::remove_file(temp_path).await;
            Err(InstallError::Io(error))
        }
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
