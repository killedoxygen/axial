use axial_launcher::{
    CRASH_ARTIFACT_EXIT_CORRELATION_WINDOW_MS, CrashArtifactKind, CrashEvidence,
    MAX_CRASH_ARTIFACT_BYTES, parse_crash_evidence,
};
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::OwnedSemaphorePermit;

const MAX_SCANNED_ENTRIES: usize = 256;
const COLLECTION_DEADLINE: Duration = Duration::from_millis(250);

#[derive(Debug)]
pub(crate) struct CrashArtifactCollectionRequest {
    game_dir: PathBuf,
    process_started_at_ms: u64,
    exit_observed_at_ms: u64,
}

impl CrashArtifactCollectionRequest {
    pub(crate) fn new(
        game_dir: PathBuf,
        process_started_at_ms: u64,
        exit_observed_at_ms: u64,
    ) -> Self {
        Self {
            game_dir,
            process_started_at_ms,
            exit_observed_at_ms,
        }
    }
}

#[derive(Debug)]
struct Candidate {
    file: File,
    name: String,
    kind: CrashArtifactKind,
    snapshot: FileSnapshot,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FileSnapshot {
    identity: platform::FileIdentity,
    len: u64,
    modified_at: SystemTime,
    change: platform::ChangeMarker,
}

pub(crate) async fn collect_crash_evidence(
    request: CrashArtifactCollectionRequest,
    permit: OwnedSemaphorePermit,
) -> Option<CrashEvidence> {
    tokio::time::timeout(
        COLLECTION_DEADLINE,
        collect_within_deadline(request, permit),
    )
    .await
    .ok()
    .flatten()
}

async fn collect_within_deadline(
    request: CrashArtifactCollectionRequest,
    permit: OwnedSemaphorePermit,
) -> Option<CrashEvidence> {
    let (kind, raw) = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        collect_blocking(request)
    })
    .await
    .ok()??;
    parse_crash_evidence(kind, &raw)
}

fn collect_blocking(
    request: CrashArtifactCollectionRequest,
) -> Option<(CrashArtifactKind, Vec<u8>)> {
    if request.process_started_at_ms > request.exit_observed_at_ms {
        return None;
    }
    let mut candidates = Vec::new();
    platform::collect_candidates(
        &request.game_dir,
        request.process_started_at_ms,
        request.exit_observed_at_ms,
        &mut candidates,
    )?;

    let candidate = newest_candidate(candidates)?;
    let kind = candidate.kind;
    let raw = read_stable_regular_prefix(candidate)?;
    Some((kind, raw))
}

fn artifact_name_matches(kind: CrashArtifactKind, name: &str) -> bool {
    match kind {
        CrashArtifactKind::MinecraftCrashReport => {
            name.starts_with("crash-") && name.ends_with(".txt")
        }
        CrashArtifactKind::JvmFatalError => {
            name.starts_with("hs_err_pid") && name.ends_with(".log")
        }
    }
}

fn timestamp_is_correlated(
    modified_at: SystemTime,
    process_started_at_ms: u64,
    exit_observed_at_ms: u64,
) -> bool {
    system_time_ms(modified_at).is_some_and(|modified_at_ms| {
        if process_started_at_ms > exit_observed_at_ms {
            return false;
        }
        let lower_bound = process_started_at_ms
            .max(exit_observed_at_ms.saturating_sub(CRASH_ARTIFACT_EXIT_CORRELATION_WINDOW_MS));
        (lower_bound..=exit_observed_at_ms).contains(&modified_at_ms)
    })
}

fn newest_candidate(candidates: Vec<Candidate>) -> Option<Candidate> {
    candidates.into_iter().max_by(candidate_order)
}

fn retain_newest_candidate(candidates: &mut Vec<Candidate>, candidate: Candidate) {
    match candidates.pop() {
        Some(current) if candidate_order(&current, &candidate).is_gt() => candidates.push(current),
        _ => candidates.push(candidate),
    }
}

fn candidate_order(left: &Candidate, right: &Candidate) -> std::cmp::Ordering {
    left.snapshot
        .modified_at
        .cmp(&right.snapshot.modified_at)
        .then_with(|| candidate_tie_break(left).cmp(&candidate_tie_break(right)))
}

fn candidate_tie_break(candidate: &Candidate) -> (u8, &str) {
    let kind = match candidate.kind {
        CrashArtifactKind::MinecraftCrashReport => 0,
        CrashArtifactKind::JvmFatalError => 1,
    };
    (kind, &candidate.name)
}

fn read_stable_regular_prefix(mut candidate: Candidate) -> Option<Vec<u8>> {
    if platform::snapshot_regular(&candidate.file)? != candidate.snapshot {
        return None;
    }

    let limit = u64::try_from(MAX_CRASH_ARTIFACT_BYTES)
        .ok()?
        .saturating_add(1);
    let mut raw = Vec::with_capacity(candidate.snapshot.len.min(limit) as usize);
    candidate
        .file
        .by_ref()
        .take(limit)
        .read_to_end(&mut raw)
        .ok()?;
    (platform::snapshot_regular(&candidate.file)? == candidate.snapshot).then_some(raw)
}

fn system_time_ms(value: SystemTime) -> Option<u64> {
    value
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| u64::try_from(duration.as_millis()).ok())
}

#[cfg(unix)]
mod platform {
    use super::*;
    use rustix::fs::{Dir, Mode, OFlags, open, openat};
    use std::os::unix::fs::MetadataExt;

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(super) struct FileIdentity {
        device: u64,
        inode: u64,
    }

    pub(super) type ChangeMarker = (i64, i64);

    pub(super) fn collect_candidates(
        game_dir: &Path,
        process_started_at_ms: u64,
        exit_observed_at_ms: u64,
        candidates: &mut Vec<Candidate>,
    ) -> Option<()> {
        let root = open(game_dir, directory_flags(), Mode::empty()).ok()?;
        if let Ok(reports) = openat(&root, "crash-reports", directory_flags(), Mode::empty()) {
            scan_directory(
                &reports,
                CrashArtifactKind::MinecraftCrashReport,
                process_started_at_ms,
                exit_observed_at_ms,
                candidates,
            );
        }
        scan_directory(
            &root,
            CrashArtifactKind::JvmFatalError,
            process_started_at_ms,
            exit_observed_at_ms,
            candidates,
        );
        Some(())
    }

    fn directory_flags() -> OFlags {
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC
    }

    fn scan_directory(
        directory: &std::os::fd::OwnedFd,
        kind: CrashArtifactKind,
        process_started_at_ms: u64,
        exit_observed_at_ms: u64,
        candidates: &mut Vec<Candidate>,
    ) {
        let Ok(mut entries) = Dir::read_from(directory) else {
            return;
        };
        let mut scanned_entries = 0;
        while scanned_entries < MAX_SCANNED_ENTRIES {
            let Some(Ok(entry)) = entries.next() else {
                return;
            };
            let name = entry.file_name();
            if name.to_bytes() == b"." || name.to_bytes() == b".." {
                continue;
            }
            scanned_entries += 1;
            let Ok(name_text) = name.to_str() else {
                continue;
            };
            if !artifact_name_matches(kind, name_text) {
                continue;
            }
            let Ok(fd) = openat(
                directory,
                name,
                OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
                Mode::empty(),
            ) else {
                continue;
            };
            let file = File::from(fd);
            let Some(snapshot) = snapshot_regular(&file) else {
                continue;
            };
            if !timestamp_is_correlated(
                snapshot.modified_at,
                process_started_at_ms,
                exit_observed_at_ms,
            ) {
                continue;
            }
            retain_newest_candidate(
                candidates,
                Candidate {
                    file,
                    name: name_text.to_owned(),
                    kind,
                    snapshot,
                },
            );
        }
    }

    pub(super) fn snapshot_regular(file: &File) -> Option<FileSnapshot> {
        let metadata = file.metadata().ok()?;
        if !metadata.file_type().is_file() {
            return None;
        }
        Some(FileSnapshot {
            identity: FileIdentity {
                device: metadata.dev(),
                inode: metadata.ino(),
            },
            len: metadata.len(),
            modified_at: metadata.modified().ok()?,
            change: (metadata.ctime(), metadata.ctime_nsec()),
        })
    }
}

#[cfg(windows)]
mod platform {
    use super::*;
    use std::mem::size_of;
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT, FILE_BASIC_INFO,
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_ID_INFO,
        FILE_LIST_DIRECTORY, FILE_READ_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE,
        FILE_STANDARD_INFO, FileBasicInfo, FileIdInfo, FileStandardInfo,
        GetFileInformationByHandleEx,
    };

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(super) struct FileIdentity {
        volume: u64,
        id: [u8; 16],
    }

    pub(super) type ChangeMarker = i64;

    pub(super) fn collect_candidates(
        game_dir: &Path,
        process_started_at_ms: u64,
        exit_observed_at_ms: u64,
        candidates: &mut Vec<Candidate>,
    ) -> Option<()> {
        let root = open_directory(game_dir)?;
        let reports_path = game_dir.join("crash-reports");
        if let Some(reports) = open_directory(&reports_path) {
            scan_directory(
                &reports_path,
                &reports,
                CrashArtifactKind::MinecraftCrashReport,
                process_started_at_ms,
                exit_observed_at_ms,
                candidates,
            );
        }
        scan_directory(
            game_dir,
            &root,
            CrashArtifactKind::JvmFatalError,
            process_started_at_ms,
            exit_observed_at_ms,
            candidates,
        );
        Some(())
    }

    pub(super) fn open_directory(path: &Path) -> Option<File> {
        let file = open_no_follow(path, FILE_LIST_DIRECTORY | FILE_READ_ATTRIBUTES)?;
        let basic: FILE_BASIC_INFO = query(&file, FileBasicInfo)?;
        let standard: FILE_STANDARD_INFO = query(&file, FileStandardInfo)?;
        ((basic.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT == 0)
            && (basic.FileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0)
            && standard.Directory)
            .then_some(file)
    }

    fn scan_directory(
        directory_path: &Path,
        held_directory: &File,
        kind: CrashArtifactKind,
        process_started_at_ms: u64,
        exit_observed_at_ms: u64,
        candidates: &mut Vec<Candidate>,
    ) {
        // `open_directory` omits FILE_SHARE_DELETE. This live handle prevents the
        // ambient directory path from being renamed or replaced during enumeration.
        let _namespace_lock = held_directory;
        let Ok(entries) = std::fs::read_dir(directory_path) else {
            return;
        };
        for entry in entries.take(MAX_SCANNED_ENTRIES).flatten() {
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if !artifact_name_matches(kind, &name) {
                continue;
            }
            let Some(file) =
                open_no_follow(&entry.path(), windows_sys::Win32::Foundation::GENERIC_READ)
            else {
                continue;
            };
            let Some(snapshot) = snapshot_regular(&file) else {
                continue;
            };
            if !timestamp_is_correlated(
                snapshot.modified_at,
                process_started_at_ms,
                exit_observed_at_ms,
            ) {
                continue;
            }
            retain_newest_candidate(
                candidates,
                Candidate {
                    file,
                    name,
                    kind,
                    snapshot,
                },
            );
        }
    }

    fn open_no_follow(path: &Path, access: u32) -> Option<File> {
        let mut options = std::fs::OpenOptions::new();
        options
            .read(true)
            .access_mode(access)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS);
        options.open(path).ok()
    }

    pub(super) fn snapshot_regular(file: &File) -> Option<FileSnapshot> {
        let basic: FILE_BASIC_INFO = query(file, FileBasicInfo)?;
        let standard: FILE_STANDARD_INFO = query(file, FileStandardInfo)?;
        if basic.FileAttributes & (FILE_ATTRIBUTE_REPARSE_POINT | FILE_ATTRIBUTE_DIRECTORY) != 0
            || standard.Directory
            || standard.EndOfFile < 0
        {
            return None;
        }
        let identity: FILE_ID_INFO = query(file, FileIdInfo)?;
        Some(FileSnapshot {
            identity: FileIdentity {
                volume: identity.VolumeSerialNumber,
                id: identity.FileId.Identifier,
            },
            len: u64::try_from(standard.EndOfFile).ok()?,
            modified_at: windows_time(basic.LastWriteTime)?,
            change: basic.ChangeTime,
        })
    }

    fn query<T: Default>(file: &File, class: i32) -> Option<T> {
        let mut value = T::default();
        let size = u32::try_from(size_of::<T>()).ok()?;
        let ok = unsafe {
            GetFileInformationByHandleEx(
                file.as_raw_handle(),
                class,
                (&mut value as *mut T).cast(),
                size,
            )
        };
        (ok != 0).then_some(value)
    }

    fn windows_time(ticks: i64) -> Option<SystemTime> {
        const WINDOWS_TO_UNIX_EPOCH_TICKS: i64 = 116_444_736_000_000_000;
        const TICKS_PER_SECOND: u64 = 10_000_000;
        let ticks = u64::try_from(ticks.checked_sub(WINDOWS_TO_UNIX_EPOCH_TICKS)?).ok()?;
        UNIX_EPOCH.checked_add(Duration::new(
            ticks / TICKS_PER_SECOND,
            u32::try_from((ticks % TICKS_PER_SECOND) * 100).ok()?,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};

    static NEXT_ROOT: AtomicU64 = AtomicU64::new(0);

    struct TestRoot(PathBuf);

    impl TestRoot {
        fn new(label: &str) -> Self {
            let sequence = NEXT_ROOT.fetch_add(1, AtomicOrdering::Relaxed);
            let root = std::env::temp_dir().join(format!(
                "axial-crash-collector-{label}-{}-{sequence}",
                std::process::id()
            ));
            fs::create_dir_all(&root).expect("create test root");
            Self(root)
        }
    }

    impl Drop for TestRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn now_ms() -> u64 {
        system_time_ms(SystemTime::now()).expect("current time")
    }

    async fn collection_permit() -> OwnedSemaphorePermit {
        std::sync::Arc::new(tokio::sync::Semaphore::new(1))
            .acquire_owned()
            .await
            .expect("collection permit")
    }

    fn test_candidate(
        file: &Path,
        name: &str,
        kind: CrashArtifactKind,
        modified_at: SystemTime,
    ) -> Candidate {
        let file = File::open(file).expect("open candidate");
        let mut snapshot = platform::snapshot_regular(&file).expect("candidate snapshot");
        snapshot.modified_at = modified_at;
        Candidate {
            file,
            name: name.to_owned(),
            kind,
            snapshot,
        }
    }

    #[test]
    fn names_and_timestamps_use_closed_correlation_rules() {
        assert!(artifact_name_matches(
            CrashArtifactKind::MinecraftCrashReport,
            "crash-2026-07-11_01.02.03-client.txt"
        ));
        assert!(artifact_name_matches(
            CrashArtifactKind::JvmFatalError,
            "hs_err_pid1234.log"
        ));
        for rejected in ["latest.log", "crash-secret.log", "hs_err_pid1.txt"] {
            assert!(!artifact_name_matches(
                CrashArtifactKind::MinecraftCrashReport,
                rejected
            ));
            assert!(!artifact_name_matches(
                CrashArtifactKind::JvmFatalError,
                rejected
            ));
        }
        let time = |millis| UNIX_EPOCH + Duration::from_millis(millis);
        assert!(timestamp_is_correlated(time(10_000), 0, 25_000));
        assert!(timestamp_is_correlated(time(25_000), 0, 25_000));
        assert!(!timestamp_is_correlated(time(9_999), 0, 25_000));
        assert!(!timestamp_is_correlated(time(25_001), 0, 25_000));
        assert!(timestamp_is_correlated(
            time(25_000) + Duration::from_nanos(1),
            0,
            25_000
        ));

        assert!(timestamp_is_correlated(time(20_000), 20_000, 25_000));
        assert!(!timestamp_is_correlated(time(19_999), 20_000, 25_000));
        assert!(!timestamp_is_correlated(time(25_000), 25_001, 25_000));
    }

    #[test]
    fn newest_selection_is_deterministic_across_artifact_kinds() {
        let root = TestRoot::new("selection");
        let file = root.0.join("candidate");
        fs::write(&file, "candidate").expect("write candidate");
        let report = test_candidate(
            &file,
            "crash-a.txt",
            CrashArtifactKind::MinecraftCrashReport,
            UNIX_EPOCH + Duration::from_nanos(100),
        );
        let hs_err = test_candidate(
            &file,
            "hs_err_pid1.log",
            CrashArtifactKind::JvmFatalError,
            UNIX_EPOCH + Duration::from_nanos(101),
        );
        assert_eq!(
            newest_candidate(vec![hs_err, report]).unwrap().kind,
            CrashArtifactKind::JvmFatalError
        );
    }

    #[tokio::test]
    async fn collection_reads_one_exact_regular_artifact_and_ignores_other_files() {
        let root = TestRoot::new("regular");
        let process_started_at_ms = now_ms().saturating_sub(1_000);
        let reports = root.0.join("crash-reports");
        fs::create_dir(&reports).expect("create reports");
        fs::write(
            reports.join("latest.log"),
            "java.lang.OutOfMemoryError: decoy",
        )
        .expect("write decoy");
        fs::write(
            reports.join("crash-2026-07-11_01.02.03-client.txt"),
            "Description: Rendering game\njava.lang.OutOfMemoryError: Java heap space",
        )
        .expect("write report");

        let evidence = collect_crash_evidence(
            CrashArtifactCollectionRequest::new(root.0.clone(), process_started_at_ms, now_ms()),
            collection_permit().await,
        )
        .await
        .expect("crash evidence");
        assert_eq!(evidence.source, CrashArtifactKind::MinecraftCrashReport);
        assert!(evidence.names_out_of_memory);
    }

    #[tokio::test]
    async fn absent_malformed_and_entry_saturated_collection_are_normal() {
        let root = TestRoot::new("absence");
        let process_started_at_ms = now_ms().saturating_sub(1_000);
        assert!(
            collect_crash_evidence(
                CrashArtifactCollectionRequest::new(
                    root.0.clone(),
                    process_started_at_ms,
                    now_ms()
                ),
                collection_permit().await
            )
            .await
            .is_none()
        );

        let reports = root.0.join("crash-reports");
        fs::create_dir(&reports).expect("create reports");
        for index in 0..MAX_SCANNED_ENTRIES + 16 {
            fs::write(reports.join(format!("unrelated-{index}.txt")), "ignored")
                .expect("write unrelated file");
        }
        fs::write(
            root.0.join("hs_err_pid42.log"),
            "# Problematic frame:\n# C  [nvoglv64.dll+0x12] SwapBuffers+0x1",
        )
        .expect("write hs_err");
        let evidence = collect_crash_evidence(
            CrashArtifactCollectionRequest::new(root.0.clone(), process_started_at_ms, now_ms()),
            collection_permit().await,
        )
        .await
        .expect("independent root budget");
        assert_eq!(evidence.source, CrashArtifactKind::JvmFatalError);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn symlinked_directory_and_artifact_are_rejected() {
        use std::os::unix::fs::symlink;

        let root = TestRoot::new("symlink");
        let outside = TestRoot::new("outside");
        let process_started_at_ms = now_ms();
        fs::write(
            outside.0.join("crash-private.txt"),
            "java.lang.OutOfMemoryError: private",
        )
        .expect("write outside report");
        symlink(&outside.0, root.0.join("crash-reports")).expect("link reports");
        symlink(
            outside.0.join("crash-private.txt"),
            root.0.join("hs_err_pid1.log"),
        )
        .expect("link hs_err");

        assert!(
            collect_crash_evidence(
                CrashArtifactCollectionRequest::new(
                    root.0.clone(),
                    process_started_at_ms,
                    now_ms()
                ),
                collection_permit().await
            )
            .await
            .is_none()
        );
    }

    #[cfg(unix)]
    #[test]
    fn retained_handle_fails_closed_after_path_replacement() {
        let root = TestRoot::new("replacement");
        let reports = root.0.join("crash-reports");
        fs::create_dir(&reports).expect("create reports");
        let path = reports.join("crash-replacement.txt");
        let process_started_at_ms = now_ms().saturating_sub(1_000);
        fs::write(
            &path,
            "Description: Rendering game\njava.lang.IllegalStateException: original",
        )
        .expect("write original");
        let exit_observed_at_ms = now_ms().saturating_add(1_000);

        let mut candidates = Vec::new();
        platform::collect_candidates(
            &root.0,
            process_started_at_ms,
            exit_observed_at_ms,
            &mut candidates,
        )
        .expect("collect candidates");
        let candidate = newest_candidate(candidates).expect("candidate");

        fs::rename(&path, reports.join("moved.txt")).expect("move original");
        fs::write(
            &path,
            "Description: Rendering game\njava.lang.OutOfMemoryError: replacement",
        )
        .expect("write replacement");
        assert!(read_stable_regular_prefix(candidate).is_none());
    }

    #[cfg(windows)]
    #[test]
    fn held_windows_directory_handle_denies_namespace_replacement() {
        let root = TestRoot::new("windows-directory-lock");
        let moved = root.0.with_extension("moved");
        let held = platform::open_directory(&root.0).expect("held root directory");

        assert!(fs::rename(&root.0, &moved).is_err());
        drop(held);
        fs::rename(&root.0, &moved).expect("rename after handle release");
        fs::rename(&moved, &root.0).expect("restore test root");
    }
}
