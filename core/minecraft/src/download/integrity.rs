use super::model::{ActualIntegrity, DownloadError};
use super::path_safety::filesystem_path;
use sha1::{Digest as _, Sha1};
use std::io::{self, Read};
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LauncherManagedArtifactReadiness {
    Missing,
    MetadataInvalid,
    MetadataMissing,
    UnsupportedExisting,
    Verified,
    Corrupt,
}

pub(super) async fn hash_file(path: &Path) -> Result<ActualIntegrity, DownloadError> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || hash_file_sync(&path))
        .await
        .map_err(blocking_join_error)?
        .map_err(DownloadError::FileOperation)
}

fn hash_file_sync(path: &Path) -> std::io::Result<ActualIntegrity> {
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
        size = size.checked_add(read as u64).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "asset hash size overflowed")
        })?;
    }

    Ok(ActualIntegrity {
        size,
        sha1: format!("{:x}", hasher.finalize()),
    })
}

pub(super) fn is_sha1_hex(value: &str) -> bool {
    value.len() == 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn blocking_join_error(error: tokio::task::JoinError) -> DownloadError {
    DownloadError::FileOperation(io::Error::other(format!(
        "blocking file task failed: {error}"
    )))
}
