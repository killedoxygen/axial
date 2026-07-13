use super::model::{ActualIntegrity, DownloadError, DownloadIntegrityError, ExpectedIntegrity};
use super::path_safety::{bounded_download_file_label, filesystem_path};
use sha1::{Digest as _, Sha1};
use std::io::{self, Read};
use std::path::Path;
#[cfg(test)]
use std::path::PathBuf;
use tokio::fs as async_fs;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ExistingArtifactIntegrity {
    Missing,
    MetadataInvalid,
    MetadataMissing,
    UnsupportedExisting,
    Verified(u64),
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
    existing_artifact_integrity_with_policy(path, expected, ExistingArtifactPolicy::FullHash).await
}

pub(super) async fn existing_content_addressed_asset_integrity(
    path: &Path,
    expected: &ExpectedIntegrity,
) -> Result<ExistingArtifactIntegrity, DownloadError> {
    existing_artifact_integrity_with_policy(
        path,
        expected,
        ExistingArtifactPolicy::ContentAddressedAsset,
    )
    .await
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExistingArtifactPolicy {
    FullHash,
    ContentAddressedAsset,
}

async fn existing_artifact_integrity_with_policy(
    path: &Path,
    expected: &ExpectedIntegrity,
    policy: ExistingArtifactPolicy,
) -> Result<ExistingArtifactIntegrity, DownloadError> {
    if expected.sha1.is_none() {
        return Ok(ExistingArtifactIntegrity::MetadataMissing);
    }
    if let Some(expected_sha1) = expected.sha1.as_deref()
        && !is_sha1_hex(expected_sha1)
    {
        return Ok(ExistingArtifactIntegrity::MetadataInvalid);
    }
    let Ok(metadata) = async_fs::symlink_metadata(filesystem_path(path).as_ref()).await else {
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

    if matches!(policy, ExistingArtifactPolicy::ContentAddressedAsset)
        && expected.size.is_some()
        && expected
            .sha1
            .as_deref()
            .is_some_and(|sha1| content_addressed_asset_path_matches(path, sha1))
    {
        // Asset objects are named by SHA-1 and verified while being downloaded.
        // Later ensure passes only prove the expected-size file is still present
        // at that content-addressed path; full readiness/repair checks still hash.
        return Ok(ExistingArtifactIntegrity::Verified(metadata.len()));
    }

    let actual = hash_file(path).await?;
    match verify_download_integrity(path, expected, &actual) {
        Ok(()) => Ok(ExistingArtifactIntegrity::Verified(actual.size)),
        Err(error) => Ok(ExistingArtifactIntegrity::Corrupt(error)),
    }
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
    let Ok(metadata) = std::fs::symlink_metadata(filesystem_path(path).as_ref()) else {
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

pub fn jar_contains_signed_metadata(path: &Path) -> bool {
    let Ok(file) = std::fs::File::open(filesystem_path(path).as_ref()) else {
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

#[cfg(test)]
pub(super) async fn existing_file_satisfies(
    path: &Path,
    expected: &ExpectedIntegrity,
) -> Result<bool, DownloadError> {
    Ok(matches!(
        existing_artifact_integrity(path, expected).await?,
        ExistingArtifactIntegrity::Verified(_)
    ))
}

#[cfg(test)]
pub(super) async fn existing_asset_object_satisfies(
    path: &Path,
    expected: &ExpectedIntegrity,
) -> Result<bool, DownloadError> {
    Ok(matches!(
        existing_content_addressed_asset_integrity(path, expected).await?,
        ExistingArtifactIntegrity::Verified(_)
    ))
}

pub(super) async fn hash_file(path: &Path) -> Result<ActualIntegrity, DownloadError> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || hash_file_sync(&path))
        .await
        .map_err(blocking_join_error)?
        .map_err(DownloadError::FileOperation)
}

fn hash_file_sync(path: &Path) -> std::io::Result<ActualIntegrity> {
    observe_hash_file_sync(path);
    let mut file = std::fs::File::open(filesystem_path(path).as_ref())?;
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

fn content_addressed_asset_path_matches(path: &Path, expected_sha1: &str) -> bool {
    let Some(file_name) = path.file_name().and_then(|value| value.to_str()) else {
        return false;
    };
    let Some(prefix) = path
        .parent()
        .and_then(|parent| parent.file_name())
        .and_then(|value| value.to_str())
    else {
        return false;
    };

    file_name.eq_ignore_ascii_case(expected_sha1)
        && prefix.eq_ignore_ascii_case(&expected_sha1[..2])
}

fn blocking_join_error(error: tokio::task::JoinError) -> DownloadError {
    DownloadError::FileOperation(io::Error::other(format!(
        "blocking file task failed: {error}"
    )))
}

#[cfg(test)]
pub(super) struct HashFileCallObserver {
    path: PathBuf,
    calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

#[cfg(test)]
impl HashFileCallObserver {
    pub(super) fn calls(&self) -> usize {
        self.calls.load(std::sync::atomic::Ordering::SeqCst)
    }
}

#[cfg(test)]
impl Drop for HashFileCallObserver {
    fn drop(&mut self) {
        let mut observer = HASH_FILE_CALL_OBSERVER
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if observer.as_ref().is_some_and(|active| {
            active.path == self.path && std::sync::Arc::ptr_eq(&active.calls, &self.calls)
        }) {
            *observer = None;
        }
    }
}

#[cfg(test)]
struct ActiveHashFileCallObserver {
    path: PathBuf,
    calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

#[cfg(test)]
static HASH_FILE_CALL_OBSERVER: std::sync::Mutex<Option<ActiveHashFileCallObserver>> =
    std::sync::Mutex::new(None);

#[cfg(test)]
pub(super) fn observe_hash_file_calls(path: &Path) -> HashFileCallObserver {
    let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut observer = HASH_FILE_CALL_OBSERVER
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    *observer = Some(ActiveHashFileCallObserver {
        path: path.to_path_buf(),
        calls: calls.clone(),
    });
    HashFileCallObserver {
        path: path.to_path_buf(),
        calls,
    }
}

#[cfg(test)]
fn observe_hash_file_sync(path: &Path) {
    let observer = HASH_FILE_CALL_OBSERVER
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(active) = observer.as_ref()
        && active.path == path
    {
        active
            .calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }
}

#[cfg(not(test))]
fn observe_hash_file_sync(_path: &Path) {}
