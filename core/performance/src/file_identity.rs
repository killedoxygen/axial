use std::fs;
use std::io;
use std::path::Path;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FileIdentity {
    #[cfg(unix)]
    Unix { device: u64, inode: u64 },
    #[cfg(windows)]
    Windows {
        volume_serial: u64,
        file_id: [u8; 16],
    },
}

pub(crate) struct AdmittedFile {
    file: fs::File,
    metadata: fs::Metadata,
    identity: FileIdentity,
}

impl AdmittedFile {
    pub(crate) fn metadata(&self) -> &fs::Metadata {
        &self.metadata
    }

    pub(crate) fn identity(&self) -> FileIdentity {
        self.identity
    }

    pub(crate) fn into_file(self) -> fs::File {
        self.file
    }
}

pub(crate) fn from_file(file: &fs::File) -> io::Result<FileIdentity> {
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(invalid_identity("managed identity is not a regular file"));
    }
    platform_file_identity(file, &metadata)
}

pub(crate) fn admit(path: &Path) -> io::Result<AdmittedFile> {
    platform_admit(path)
}

pub(crate) async fn admit_async(path: &Path) -> io::Result<AdmittedFile> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || admit(&path))
        .await
        .map_err(|_| io::Error::other("managed identity task stopped"))?
}

pub(crate) fn revalidate(path: &Path, expected: FileIdentity, expected_len: u64) -> io::Result<()> {
    platform_revalidate(path, expected, expected_len)
}

pub(crate) async fn revalidate_async(
    path: &Path,
    expected: FileIdentity,
    expected_len: u64,
) -> io::Result<()> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || revalidate(&path, expected, expected_len))
        .await
        .map_err(|_| io::Error::other("managed identity task stopped"))?
}

#[cfg(unix)]
fn platform_admit(path: &Path) -> io::Result<AdmittedFile> {
    use std::os::unix::fs::MetadataExt;

    let before = fs::symlink_metadata(path)?;
    if !before.file_type().is_file() {
        return Err(invalid_identity("managed identity is not a regular file"));
    }
    let file = fs::File::open(path)?;
    let metadata = file.metadata()?;
    let after = fs::symlink_metadata(path)?;
    let identity = platform_file_identity(&file, &metadata)?;
    if !metadata.is_file()
        || !after.file_type().is_file()
        || before.dev() != metadata.dev()
        || before.ino() != metadata.ino()
        || after.dev() != metadata.dev()
        || after.ino() != metadata.ino()
        || before.len() != metadata.len()
        || after.len() != metadata.len()
    {
        return Err(invalid_identity("managed identity changed while opening"));
    }
    Ok(AdmittedFile {
        file,
        metadata,
        identity,
    })
}

#[cfg(windows)]
fn platform_admit(path: &Path) -> io::Result<AdmittedFile> {
    let file = open_regular_no_follow(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(invalid_identity("managed identity is not a regular file"));
    }
    let identity = platform_file_identity(&file, &metadata)?;
    platform_revalidate(path, identity, metadata.len())?;
    Ok(AdmittedFile {
        file,
        metadata,
        identity,
    })
}

#[cfg(not(any(unix, windows)))]
fn platform_admit(_path: &Path) -> io::Result<AdmittedFile> {
    Err(unsupported_identity())
}

#[cfg(unix)]
fn platform_revalidate(path: &Path, expected: FileIdentity, expected_len: u64) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt;

    let metadata = fs::symlink_metadata(path)?;
    let FileIdentity::Unix { device, inode } = expected;
    if !metadata.file_type().is_file()
        || metadata.len() != expected_len
        || metadata.dev() != device
        || metadata.ino() != inode
    {
        return Err(invalid_identity("managed identity changed after admission"));
    }
    Ok(())
}

#[cfg(windows)]
fn platform_revalidate(path: &Path, expected: FileIdentity, expected_len: u64) -> io::Result<()> {
    let first = open_regular_no_follow(path)?;
    let first_metadata = first.metadata()?;
    if !first_metadata.is_file()
        || first_metadata.len() != expected_len
        || platform_file_identity(&first, &first_metadata)? != expected
    {
        return Err(invalid_identity("managed identity changed after admission"));
    }

    let between = fs::symlink_metadata(path)?;
    if !between.file_type().is_file() || between.len() != expected_len {
        return Err(invalid_identity("managed identity changed after admission"));
    }
    let second = open_regular_no_follow(path)?;
    let second_metadata = second.metadata()?;
    if !second_metadata.is_file()
        || second_metadata.len() != expected_len
        || platform_file_identity(&second, &second_metadata)? != expected
    {
        return Err(invalid_identity("managed identity changed after admission"));
    }
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn platform_revalidate(
    _path: &Path,
    _expected: FileIdentity,
    _expected_len: u64,
) -> io::Result<()> {
    Err(unsupported_identity())
}

#[cfg(unix)]
fn platform_file_identity(_file: &fs::File, metadata: &fs::Metadata) -> io::Result<FileIdentity> {
    use std::os::unix::fs::MetadataExt;

    Ok(FileIdentity::Unix {
        device: metadata.dev(),
        inode: metadata.ino(),
    })
}

#[cfg(windows)]
fn platform_file_identity(file: &fs::File, _metadata: &fs::Metadata) -> io::Result<FileIdentity> {
    use std::mem::size_of;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ID_INFO, FileIdInfo, GetFileInformationByHandleEx,
    };

    let mut info = FILE_ID_INFO::default();
    // SAFETY: `file` owns a valid handle, and `info` is a correctly sized writable buffer.
    let succeeded = unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle() as HANDLE,
            FileIdInfo,
            (&raw mut info).cast(),
            size_of::<FILE_ID_INFO>() as u32,
        )
    };
    if succeeded == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(FileIdentity::Windows {
        volume_serial: info.VolumeSerialNumber,
        file_id: info.FileId.Identifier,
    })
}

#[cfg(windows)]
fn open_regular_no_follow(path: &Path) -> io::Result<fs::File> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    };

    fs::OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
}

fn invalid_identity(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[cfg(not(any(unix, windows)))]
fn platform_file_identity(_file: &fs::File, _metadata: &fs::Metadata) -> io::Result<FileIdentity> {
    Err(unsupported_identity())
}

#[cfg(not(any(unix, windows)))]
fn unsupported_identity() -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        "exact managed file identity is unavailable on this platform",
    )
}

#[cfg(all(test, any(unix, windows)))]
mod tests {
    use super::{admit, revalidate};
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn test_dir(label: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "axial-performance-file-identity-{label}-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("test directory");
        path
    }

    #[test]
    fn open_file_and_hardlink_share_exact_identity() {
        let root = test_dir("hardlink");
        let source = root.join("source");
        let alias = root.join("alias");
        fs::write(&source, b"same bytes").expect("source");
        fs::hard_link(&source, &alias).expect("hardlink");

        let admitted = admit(&source).expect("source admission");
        revalidate(&source, admitted.identity(), admitted.metadata().len()).unwrap();
        revalidate(&alias, admitted.identity(), admitted.metadata().len()).unwrap();

        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn distinct_same_bytes_files_have_distinct_identity() {
        let root = test_dir("distinct");
        let left = root.join("left");
        let right = root.join("right");
        fs::write(&left, b"same bytes").expect("left");
        fs::write(&right, b"same bytes").expect("right");

        let admitted = admit(&left).expect("left admission");
        assert!(revalidate(&right, admitted.identity(), admitted.metadata().len()).is_err());

        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn same_length_path_replacement_fails_revalidation() {
        let root = test_dir("replacement");
        let path = root.join("managed");
        let parked = root.join("parked");
        fs::write(&path, b"same bytes").expect("original");
        let admitted = admit(&path).expect("original admission");
        fs::rename(&path, &parked).expect("park original");
        fs::write(&path, b"same bytes").expect("replacement");

        assert!(revalidate(&path, admitted.identity(), admitted.metadata().len()).is_err());

        fs::remove_dir_all(root).expect("cleanup");
    }

    #[cfg(unix)]
    #[test]
    fn symlink_cannot_be_admitted_as_a_regular_identity() {
        use std::os::unix::fs::symlink;

        let root = test_dir("symlink");
        let target = root.join("target");
        let link = root.join("link");
        fs::write(&target, b"target").expect("target");
        symlink(&target, &link).expect("symlink");

        assert!(admit(&link).is_err());

        fs::remove_dir_all(root).expect("cleanup");
    }
}
