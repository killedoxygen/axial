use super::model::{ActualIntegrity, DownloadError, DownloadIntegrityError, ExpectedIntegrity};
use super::path_safety::bounded_download_file_label;
use sha1::{Digest as _, Sha1};
use std::path::Path;
use tokio::fs as async_fs;
use tokio::io::AsyncReadExt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ExistingArtifactIntegrity {
    Missing,
    UnsupportedExisting,
    Verified,
    Corrupt(DownloadIntegrityError),
}

pub(super) async fn existing_artifact_integrity(
    path: &Path,
    expected: &ExpectedIntegrity,
) -> Result<ExistingArtifactIntegrity, DownloadError> {
    let Ok(metadata) = async_fs::symlink_metadata(path).await else {
        return Ok(ExistingArtifactIntegrity::Missing);
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Ok(ExistingArtifactIntegrity::UnsupportedExisting);
    }
    if !expected.has_evidence() {
        return Ok(ExistingArtifactIntegrity::Verified);
    }
    if let Some(expected_size) = expected.size
        && metadata.len() != expected_size
    {
        return Ok(ExistingArtifactIntegrity::Corrupt(
            DownloadIntegrityError::SizeMismatch {
                file: bounded_download_file_label(path),
                expected: expected_size,
                actual: metadata.len(),
            },
        ));
    }
    if expected.sha1.is_some() {
        let actual = hash_file(path).await?;
        return match verify_download_integrity(path, expected, &actual) {
            Ok(()) => Ok(ExistingArtifactIntegrity::Verified),
            Err(error) => Ok(ExistingArtifactIntegrity::Corrupt(error)),
        };
    }
    Ok(ExistingArtifactIntegrity::Verified)
}

#[cfg(test)]
pub(super) async fn existing_file_satisfies(
    path: &Path,
    expected: &ExpectedIntegrity,
) -> Result<bool, DownloadError> {
    Ok(matches!(
        existing_artifact_integrity(path, expected).await?,
        ExistingArtifactIntegrity::Verified
    ))
}

#[cfg(test)]
pub(super) async fn existing_asset_object_satisfies(
    path: &Path,
    expected: &ExpectedIntegrity,
) -> Result<bool, DownloadError> {
    existing_file_satisfies(path, expected).await
}

pub(super) async fn hash_file(path: &Path) -> Result<ActualIntegrity, DownloadError> {
    let mut file = async_fs::File::open(path).await?;
    let mut hasher = Sha1::new();
    let mut size = 0_u64;
    let mut buffer = vec![0_u8; 64 * 1024];

    loop {
        let read = file.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        size += read as u64;
    }

    Ok(ActualIntegrity {
        size,
        sha1: Some(format!("{:x}", hasher.finalize())),
    })
}

pub(super) fn verify_download_integrity(
    path: &Path,
    expected: &ExpectedIntegrity,
    actual: &ActualIntegrity,
) -> Result<(), DownloadIntegrityError> {
    let file = bounded_download_file_label(path);
    if let Some(expected_size) = expected.size
        && actual.size != expected_size
    {
        return Err(DownloadIntegrityError::SizeMismatch {
            file,
            expected: expected_size,
            actual: actual.size,
        });
    }
    if let Some(expected_sha1) = expected.sha1.as_deref() {
        let Some(actual_sha1) = actual.sha1.as_deref() else {
            return Err(DownloadIntegrityError::MissingSha1 { file });
        };
        if !actual_sha1.eq_ignore_ascii_case(expected_sha1) {
            return Err(DownloadIntegrityError::Sha1Mismatch {
                file,
                expected: expected_sha1.to_string(),
                actual: actual_sha1.to_string(),
            });
        }
    }
    Ok(())
}

pub(super) fn download_size_mismatch(path: &Path, expected: u64, actual: u64) -> DownloadError {
    DownloadError::Integrity(
        DownloadIntegrityError::SizeMismatch {
            file: bounded_download_file_label(path),
            expected,
            actual,
        }
        .to_string(),
    )
}

pub(super) fn is_sha1_hex(value: &str) -> bool {
    value.len() == 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}
