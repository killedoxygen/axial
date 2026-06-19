use super::model::{ActualIntegrity, DownloadError, DownloadIntegrityError, ExpectedIntegrity};
use super::path_safety::bounded_download_file_label;
use sha1::{Digest as _, Sha1};
use std::io::Read;
use std::path::Path;
use tokio::fs as async_fs;
use tokio::io::AsyncReadExt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ExistingArtifactIntegrity {
    Missing,
    MetadataInvalid,
    MetadataMissing,
    UnsupportedExisting,
    Verified,
    Corrupt(DownloadIntegrityError),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LauncherManagedArtifactReadiness {
    Missing,
    MetadataInvalid,
    MetadataMissing,
    UnsupportedExisting,
    Verified,
    Corrupt,
}

pub(super) async fn existing_artifact_integrity(
    path: &Path,
    expected: &ExpectedIntegrity,
) -> Result<ExistingArtifactIntegrity, DownloadError> {
    if expected.sha1.is_none() {
        return Ok(ExistingArtifactIntegrity::MetadataMissing);
    }
    if let Some(expected_sha1) = expected.sha1.as_deref()
        && !is_sha1_hex(expected_sha1)
    {
        return Ok(ExistingArtifactIntegrity::MetadataInvalid);
    }
    let Ok(metadata) = async_fs::symlink_metadata(path).await else {
        return Ok(ExistingArtifactIntegrity::Missing);
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Ok(ExistingArtifactIntegrity::UnsupportedExisting);
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

pub fn verify_existing_launcher_managed_artifact(
    path: &Path,
    expected: &ExpectedIntegrity,
) -> LauncherManagedArtifactReadiness {
    if expected.sha1.is_none() {
        return LauncherManagedArtifactReadiness::MetadataMissing;
    }
    if let Some(expected_sha1) = expected.sha1.as_deref()
        && !is_sha1_hex(expected_sha1)
    {
        return LauncherManagedArtifactReadiness::MetadataInvalid;
    }
    let Ok(metadata) = std::fs::symlink_metadata(path) else {
        return LauncherManagedArtifactReadiness::Missing;
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return LauncherManagedArtifactReadiness::UnsupportedExisting;
    }
    if let Some(expected_size) = expected.size
        && metadata.len() != expected_size
    {
        return LauncherManagedArtifactReadiness::Corrupt;
    }
    let actual = match hash_file_sync(path) {
        Ok(actual) => actual,
        Err(_) => return LauncherManagedArtifactReadiness::Corrupt,
    };
    match verify_download_integrity(path, expected, &actual) {
        Ok(()) => LauncherManagedArtifactReadiness::Verified,
        Err(_) => LauncherManagedArtifactReadiness::Corrupt,
    }
}

pub fn verify_existing_launcher_managed_artifact_allowing_missing_checksum(
    path: &Path,
    expected: &ExpectedIntegrity,
) -> LauncherManagedArtifactReadiness {
    if expected.sha1.is_some() {
        return verify_existing_launcher_managed_artifact(path, expected);
    }
    let Ok(metadata) = std::fs::symlink_metadata(path) else {
        return LauncherManagedArtifactReadiness::Missing;
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return LauncherManagedArtifactReadiness::UnsupportedExisting;
    }
    if metadata.len() == 0 {
        return LauncherManagedArtifactReadiness::Corrupt;
    }
    if let Some(expected_size) = expected.size
        && metadata.len() != expected_size
    {
        return LauncherManagedArtifactReadiness::Corrupt;
    }
    if path
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("jar"))
        && !checksumless_jar_is_readable(path)
    {
        return LauncherManagedArtifactReadiness::Corrupt;
    }
    LauncherManagedArtifactReadiness::Verified
}

pub fn jar_contains_signed_metadata(path: &Path) -> bool {
    let Ok(file) = std::fs::File::open(path) else {
        return false;
    };
    let Ok(mut archive) = zip::ZipArchive::new(file) else {
        return false;
    };
    for index in 0..archive.len() {
        let Ok(entry) = archive.by_index(index) else {
            return false;
        };
        if signed_jar_metadata_entry_name(entry.name()) {
            return true;
        }
    }
    false
}

pub fn signed_jar_metadata_entry_name(name: &str) -> bool {
    let upper = name.replace('\\', "/").to_ascii_uppercase();
    upper == "META-INF/MANIFEST.MF"
        || (upper.starts_with("META-INF/")
            && (upper.ends_with(".SF") || upper.ends_with(".RSA") || upper.ends_with(".DSA")))
}

fn checksumless_jar_is_readable(path: &Path) -> bool {
    let Ok(file) = std::fs::File::open(path) else {
        return false;
    };
    let Ok(mut archive) = zip::ZipArchive::new(file) else {
        return false;
    };
    for index in 0..archive.len() {
        let Ok(entry) = archive.by_index(index) else {
            return false;
        };
        if !entry.is_dir() {
            return true;
        }
    }
    false
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

fn hash_file_sync(path: &Path) -> std::io::Result<ActualIntegrity> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha1::new();
    let mut size = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];

    loop {
        let read = file.read(&mut buffer)?;
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
