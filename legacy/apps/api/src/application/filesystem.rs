use axial_resource::{
    PhysicalIoClass, PhysicalWorkAdmission, PhysicalWorkRequest, process_physical_work,
};
use std::{
    ffi::OsString,
    fs,
    io::{self, ErrorKind},
    path::{Path, PathBuf},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct FilesystemScanLimits {
    pub max_depth: usize,
    pub max_entries: usize,
    pub max_bytes: u64,
}

#[derive(Debug)]
pub(crate) enum FilesystemScanError {
    Io(io::Error),
    Link,
    UnsupportedEntry,
    NotDirectory,
    DepthLimit,
    EntryLimit,
    ByteLimit,
}

impl FilesystemScanError {
    pub(crate) fn is_capacity_limit(&self) -> bool {
        matches!(self, Self::DepthLimit | Self::EntryLimit | Self::ByteLimit)
    }

    pub(crate) fn is_unsupported_layout(&self) -> bool {
        matches!(
            self,
            Self::Link | Self::UnsupportedEntry | Self::NotDirectory
        )
    }
}

impl From<io::Error> for FilesystemScanError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FilesystemEntryKind {
    Directory,
    File,
}

#[derive(Debug)]
pub(crate) struct FilesystemEntry {
    pub path: PathBuf,
    pub name: OsString,
    pub kind: FilesystemEntryKind,
    pub metadata: fs::Metadata,
}

#[derive(Debug)]
pub(crate) struct FilesystemScanBudget {
    limits: FilesystemScanLimits,
    entries: usize,
    bytes: u64,
}

impl FilesystemScanBudget {
    pub(crate) fn new(limits: FilesystemScanLimits) -> Self {
        Self {
            limits,
            entries: 0,
            bytes: 0,
        }
    }

    pub(crate) fn read_optional_directory(
        &mut self,
        directory: &Path,
    ) -> Result<Vec<FilesystemEntry>, FilesystemScanError> {
        match fs::symlink_metadata(directory) {
            Ok(metadata) => validate_directory_metadata(&metadata)?,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error.into()),
        }
        self.read_directory_entries(directory)
    }

    pub(crate) fn directory_size(&mut self, directory: &Path) -> Result<u64, FilesystemScanError> {
        let metadata = fs::symlink_metadata(directory)?;
        validate_directory_metadata(&metadata)?;
        let initial_bytes = self.bytes;
        let mut pending = vec![(directory.to_path_buf(), 0_usize)];

        while let Some((current, depth)) = pending.pop() {
            for entry in self.read_directory_entries(&current)? {
                match entry.kind {
                    FilesystemEntryKind::Directory => {
                        let next_depth = depth.saturating_add(1);
                        if next_depth > self.limits.max_depth {
                            return Err(FilesystemScanError::DepthLimit);
                        }
                        pending.push((entry.path, next_depth));
                    }
                    FilesystemEntryKind::File => self.account_bytes(entry.metadata.len())?,
                }
            }
        }

        Ok(self.bytes.saturating_sub(initial_bytes))
    }

    pub(crate) fn account_file_bytes(&mut self, bytes: u64) -> Result<(), FilesystemScanError> {
        self.account_bytes(bytes)
    }

    fn read_directory_entries(
        &mut self,
        directory: &Path,
    ) -> Result<Vec<FilesystemEntry>, FilesystemScanError> {
        let mut entries = Vec::new();
        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            self.entries = self.entries.saturating_add(1);
            if self.entries > self.limits.max_entries {
                return Err(FilesystemScanError::EntryLimit);
            }

            let path = entry.path();
            let metadata = fs::symlink_metadata(&path)?;
            let file_type = metadata.file_type();
            let kind = if file_type.is_symlink() {
                return Err(FilesystemScanError::Link);
            } else if file_type.is_dir() {
                FilesystemEntryKind::Directory
            } else if file_type.is_file() {
                FilesystemEntryKind::File
            } else {
                return Err(FilesystemScanError::UnsupportedEntry);
            };
            entries.push(FilesystemEntry {
                path,
                name: entry.file_name(),
                kind,
                metadata,
            });
        }
        Ok(entries)
    }

    fn account_bytes(&mut self, bytes: u64) -> Result<(), FilesystemScanError> {
        self.bytes = self
            .bytes
            .checked_add(bytes)
            .ok_or(FilesystemScanError::ByteLimit)?;
        if self.bytes > self.limits.max_bytes {
            return Err(FilesystemScanError::ByteLimit);
        }
        Ok(())
    }
}

fn validate_directory_metadata(metadata: &fs::Metadata) -> Result<(), FilesystemScanError> {
    if metadata.file_type().is_symlink() {
        Err(FilesystemScanError::Link)
    } else if metadata.is_dir() {
        Ok(())
    } else {
        Err(FilesystemScanError::NotDirectory)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct BlockingFilesystemTaskError;

pub(crate) struct BlockingFilesystemAdmission {
    admission: PhysicalWorkAdmission,
}

impl BlockingFilesystemAdmission {
    async fn acquire() -> Result<Self, BlockingFilesystemTaskError> {
        let admission = process_physical_work()
            .admit(PhysicalWorkRequest::foreground(
                PhysicalIoClass::Metadata,
                0,
            ))
            .await
            .map_err(|_| BlockingFilesystemTaskError)?;
        Ok(Self { admission })
    }

    async fn acquire_exclusive() -> Result<Self, BlockingFilesystemTaskError> {
        let admission = process_physical_work()
            .admit(PhysicalWorkRequest::foreground(PhysicalIoClass::Heavy, 0))
            .await
            .map_err(|_| BlockingFilesystemTaskError)?;
        Ok(Self { admission })
    }

    async fn acquire_read(scratch_bytes: u64) -> Result<Self, BlockingFilesystemTaskError> {
        let admission = process_physical_work()
            .admit(PhysicalWorkRequest::foreground(
                PhysicalIoClass::Read,
                scratch_bytes,
            ))
            .await
            .map_err(|_| BlockingFilesystemTaskError)?;
        Ok(Self { admission })
    }

    pub(crate) async fn run<T, Work>(self, work: Work) -> Result<T, BlockingFilesystemTaskError>
    where
        T: Send + 'static,
        Work: FnOnce() -> T + Send + 'static,
    {
        self.admission
            .run(move |_| work())
            .await
            .map_err(|_| BlockingFilesystemTaskError)
    }
}

pub(crate) async fn admit_blocking_filesystem()
-> Result<BlockingFilesystemAdmission, BlockingFilesystemTaskError> {
    BlockingFilesystemAdmission::acquire().await
}

pub(crate) async fn admit_exclusive_blocking_filesystem()
-> Result<BlockingFilesystemAdmission, BlockingFilesystemTaskError> {
    BlockingFilesystemAdmission::acquire_exclusive().await
}

pub(crate) async fn run_blocking_filesystem<T, Work>(
    work: Work,
) -> Result<T, BlockingFilesystemTaskError>
where
    T: Send + 'static,
    Work: FnOnce() -> T + Send + 'static,
{
    admit_blocking_filesystem().await?.run(work).await
}

pub(crate) async fn run_bounded_filesystem_read<T, Work>(
    scratch_bytes: u64,
    work: Work,
) -> Result<T, BlockingFilesystemTaskError>
where
    T: Send + 'static,
    Work: FnOnce() -> T + Send + 'static,
{
    BlockingFilesystemAdmission::acquire_read(scratch_bytes)
        .await?
        .run(work)
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Condvar, Mutex};
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn bounded_filesystem_directory_size_rejects_entry_and_byte_overflow() {
        let root = test_root("limits");
        fs::write(root.join("first"), [0_u8; 4]).expect("write first file");
        fs::write(root.join("second"), [0_u8; 4]).expect("write second file");

        let mut entry_budget = FilesystemScanBudget::new(FilesystemScanLimits {
            max_depth: 4,
            max_entries: 1,
            max_bytes: 16,
        });
        assert!(matches!(
            entry_budget.directory_size(&root),
            Err(FilesystemScanError::EntryLimit)
        ));

        let mut byte_budget = FilesystemScanBudget::new(FilesystemScanLimits {
            max_depth: 4,
            max_entries: 4,
            max_bytes: 7,
        });
        assert!(matches!(
            byte_budget.directory_size(&root),
            Err(FilesystemScanError::ByteLimit)
        ));

        fs::remove_dir_all(root).expect("remove test root");
    }

    #[cfg(unix)]
    #[test]
    fn bounded_filesystem_directory_size_rejects_symlink_cycle_without_following_it() {
        use std::os::unix::fs::symlink;

        let root = test_root("link-cycle");
        fs::create_dir_all(root.join("nested")).expect("create nested directory");
        symlink(&root, root.join("nested").join("cycle")).expect("create cycle link");
        let mut budget = FilesystemScanBudget::new(FilesystemScanLimits {
            max_depth: 8,
            max_entries: 8,
            max_bytes: 1024,
        });

        assert!(matches!(
            budget.directory_size(&root),
            Err(FilesystemScanError::Link)
        ));

        fs::remove_dir_all(root).expect("remove test root");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn native_filesystem_contract_cross_owner_reserves_foreground_capacity() {
        let owner = process_physical_work();
        let background = owner.group();
        let queued = owner.group();
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let mut tasks = Vec::new();
        let mut starts = Vec::new();
        for _ in 0..2 {
            let background = background.clone();
            let gate = Arc::clone(&gate);
            let (started_tx, started_rx) = tokio::sync::oneshot::channel();
            starts.push(started_rx);
            tasks.push(tokio::spawn(async move {
                background
                    .run(
                        PhysicalWorkRequest::background(PhysicalIoClass::Read, 0),
                        move |_| {
                            let _ = started_tx.send(());
                            let (lock, wake) = &*gate;
                            let released = lock.lock().expect("lock background worker");
                            drop(
                                wake.wait_while(released, |released| !*released)
                                    .expect("wait for background release"),
                            );
                        },
                    )
                    .await
            }));
        }
        for started in starts {
            started.await.expect("background worker started");
        }

        let queued_task = tokio::spawn({
            let queued = queued.clone();
            async move {
                queued
                    .run(
                        PhysicalWorkRequest::background(PhysicalIoClass::Read, 0),
                        |_| (),
                    )
                    .await
            }
        });
        tokio::task::yield_now().await;
        assert_eq!(
            BlockingFilesystemAdmission::acquire()
                .await
                .expect("foreground filesystem admission")
                .run(|| 11_u8)
                .await,
            Ok(11)
        );
        queued.cancel();
        assert!(queued_task.await.expect("join queued worker").is_err());

        let (lock, wake) = &*gate;
        *lock.lock().expect("release background workers") = true;
        wake.notify_all();
        for task in tasks {
            assert_eq!(task.await.expect("join background worker"), Ok(()));
        }
    }

    fn test_root(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "axial-filesystem-{name}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|value| value.as_nanos())
                .unwrap_or_default()
        ));
        fs::create_dir_all(&root).expect("create test root");
        root
    }
}
