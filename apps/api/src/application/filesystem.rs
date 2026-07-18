use std::{
    ffi::OsString,
    fs,
    io::{self, ErrorKind},
    path::{Path, PathBuf},
    sync::{Arc, OnceLock},
};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

const FILESYSTEM_TASK_CONCURRENCY: usize = 4;

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

    pub(crate) fn read_directory(
        &mut self,
        directory: &Path,
    ) -> Result<Vec<FilesystemEntry>, FilesystemScanError> {
        let metadata = fs::symlink_metadata(directory)?;
        validate_directory_metadata(&metadata)?;
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
    _global_permit: OwnedSemaphorePermit,
    _exclusive_permit: Option<OwnedSemaphorePermit>,
}

impl BlockingFilesystemAdmission {
    async fn acquire(gate: Arc<Semaphore>) -> Result<Self, BlockingFilesystemTaskError> {
        let global_permit = gate
            .acquire_owned()
            .await
            .map_err(|_| BlockingFilesystemTaskError)?;
        Ok(Self {
            _global_permit: global_permit,
            _exclusive_permit: None,
        })
    }

    async fn acquire_exclusive(
        global_gate: Arc<Semaphore>,
        exclusive_gate: Arc<Semaphore>,
    ) -> Result<Self, BlockingFilesystemTaskError> {
        let exclusive_permit = exclusive_gate
            .acquire_owned()
            .await
            .map_err(|_| BlockingFilesystemTaskError)?;
        let global_permit = global_gate
            .acquire_owned()
            .await
            .map_err(|_| BlockingFilesystemTaskError)?;
        Ok(Self {
            _global_permit: global_permit,
            _exclusive_permit: Some(exclusive_permit),
        })
    }

    pub(crate) async fn run<T, Work>(self, work: Work) -> Result<T, BlockingFilesystemTaskError>
    where
        T: Send + 'static,
        Work: FnOnce() -> T + Send + 'static,
    {
        tokio::task::spawn_blocking(move || {
            let _admission = self;
            work()
        })
        .await
        .map_err(|_| BlockingFilesystemTaskError)
    }
}

pub(crate) async fn admit_blocking_filesystem()
-> Result<BlockingFilesystemAdmission, BlockingFilesystemTaskError> {
    BlockingFilesystemAdmission::acquire(filesystem_task_gate()).await
}

pub(crate) async fn admit_exclusive_blocking_filesystem()
-> Result<BlockingFilesystemAdmission, BlockingFilesystemTaskError> {
    BlockingFilesystemAdmission::acquire_exclusive(
        filesystem_task_gate(),
        exclusive_filesystem_task_gate(),
    )
    .await
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

fn filesystem_task_gate() -> Arc<Semaphore> {
    static GATE: OnceLock<Arc<Semaphore>> = OnceLock::new();
    Arc::clone(GATE.get_or_init(|| Arc::new(Semaphore::new(FILESYSTEM_TASK_CONCURRENCY))))
}

fn exclusive_filesystem_task_gate() -> Arc<Semaphore> {
    static GATE: OnceLock<Arc<Semaphore>> = OnceLock::new();
    Arc::clone(GATE.get_or_init(|| Arc::new(Semaphore::new(1))))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        sync::{
            atomic::{AtomicBool, Ordering},
            mpsc,
        },
        time::{Duration, SystemTime, UNIX_EPOCH},
    };

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

    #[tokio::test(flavor = "current_thread")]
    async fn bounded_filesystem_work_does_not_stall_async_heartbeat() {
        let (started_tx, started_rx) = mpsc::sync_channel(1);
        let (release_tx, release_rx) = mpsc::sync_channel(1);
        let gate = Arc::new(Semaphore::new(1));
        let task = tokio::spawn(async move {
            BlockingFilesystemAdmission::acquire(gate)
                .await
                .expect("admit blocking work")
                .run(move || {
                    started_tx.send(()).expect("signal blocking task start");
                    release_rx.recv().expect("release blocking task");
                    7_u8
                })
                .await
        });

        tokio::time::timeout(Duration::from_secs(2), async move {
            tokio::task::spawn_blocking(move || started_rx.recv())
                .await
                .expect("join start observer")
                .expect("observe blocking task start");
        })
        .await
        .expect("blocking task should start");
        tokio::time::timeout(
            Duration::from_millis(250),
            tokio::time::sleep(Duration::from_millis(1)),
        )
        .await
        .expect("runtime heartbeat should progress");
        release_tx.send(()).expect("release blocking task");

        assert_eq!(
            task.await
                .expect("join async wrapper")
                .expect("blocking filesystem task"),
            7
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn filesystem_capacity_is_admitted_before_semantic_ownership() {
        let gate = Arc::new(Semaphore::new(1));
        let active = BlockingFilesystemAdmission::acquire(gate.clone())
            .await
            .expect("occupy filesystem capacity");
        let (semantic_tx, mut semantic_rx) = tokio::sync::oneshot::channel();
        let queued = tokio::spawn(async move {
            let admission = BlockingFilesystemAdmission::acquire(gate)
                .await
                .expect("admit queued filesystem work");
            semantic_tx.send(()).expect("claim semantic ownership");
            admission.run(|| ()).await.expect("run admitted work");
        });

        assert!(
            tokio::time::timeout(Duration::from_millis(25), &mut semantic_rx)
                .await
                .is_err(),
            "semantic ownership must wait for filesystem capacity"
        );
        drop(active);
        semantic_rx.await.expect("semantic ownership begins");
        queued.await.expect("join queued filesystem work");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn exclusive_admission_respects_global_capacity_before_semantic_ownership() {
        let global_gate = Arc::new(Semaphore::new(2));
        let exclusive_gate = Arc::new(Semaphore::new(1));
        let first_general = BlockingFilesystemAdmission::acquire(global_gate.clone())
            .await
            .expect("admit first general task");
        let second_general = BlockingFilesystemAdmission::acquire(global_gate.clone())
            .await
            .expect("admit second general task");
        let (semantic_tx, mut semantic_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let queued_global_gate = global_gate.clone();
        let observed_exclusive_gate = exclusive_gate.clone();
        let queued = tokio::spawn(async move {
            let admission =
                BlockingFilesystemAdmission::acquire_exclusive(queued_global_gate, exclusive_gate)
                    .await
                    .expect("admit exclusive task");
            semantic_tx.send(()).expect("claim semantic ownership");
            let _ = release_rx.await;
            drop(admission);
        });

        assert!(
            tokio::time::timeout(Duration::from_millis(25), &mut semantic_rx)
                .await
                .is_err(),
            "exclusive semantic ownership must wait for global capacity"
        );
        drop(first_general);
        semantic_rx
            .await
            .expect("exclusive semantic ownership begins");
        assert_eq!(global_gate.available_permits(), 0);
        assert_eq!(observed_exclusive_gate.available_permits(), 0);
        assert!(
            tokio::time::timeout(
                Duration::from_millis(25),
                BlockingFilesystemAdmission::acquire(global_gate.clone()),
            )
            .await
            .is_err(),
            "exclusive work must consume global filesystem capacity"
        );

        release_tx.send(()).expect("release exclusive task");
        queued.await.expect("join exclusive task");
        drop(second_general);
        assert_eq!(global_gate.available_permits(), 2);
        assert_eq!(observed_exclusive_gate.available_permits(), 1);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn admitted_filesystem_work_survives_caller_cancellation() {
        let gate = Arc::new(Semaphore::new(1));
        let admission = BlockingFilesystemAdmission::acquire(gate.clone())
            .await
            .expect("admit filesystem work");
        let (started_tx, started_rx) = mpsc::sync_channel(1);
        let (release_tx, release_rx) = mpsc::sync_channel(1);
        let completed = Arc::new(AtomicBool::new(false));
        let completed_by_work = completed.clone();
        let caller = tokio::spawn(async move {
            admission
                .run(move || {
                    started_tx.send(()).expect("signal blocking task start");
                    release_rx.recv().expect("release blocking task");
                    completed_by_work.store(true, Ordering::Release);
                })
                .await
        });

        tokio::time::timeout(Duration::from_secs(2), async move {
            tokio::task::spawn_blocking(move || started_rx.recv())
                .await
                .expect("join start observer")
                .expect("observe blocking task start");
        })
        .await
        .expect("blocking task should start");
        caller.abort();
        assert!(caller.await.expect_err("cancel caller").is_cancelled());
        release_tx.send(()).expect("release blocking task");

        let recovered = tokio::time::timeout(
            Duration::from_secs(2),
            BlockingFilesystemAdmission::acquire(gate),
        )
        .await
        .expect("detached work releases filesystem capacity")
        .expect("readmit filesystem work");
        assert!(completed.load(Ordering::Acquire));
        drop(recovered);
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
