use axial_launcher::{
    CRASH_ARTIFACT_EXIT_CORRELATION_WINDOW_MS, CrashArtifactKind, CrashEvidence,
    MAX_CRASH_ARTIFACT_BYTES, parse_crash_evidence,
};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::io::AsyncReadExt;

const MAX_SCANNED_ENTRIES: usize = 256;
const COLLECTION_DEADLINE: Duration = Duration::from_millis(250);

#[derive(Debug)]
pub(crate) struct CrashArtifactCollectionRequest {
    game_dir: PathBuf,
    exit_observed_at_ms: u64,
}

impl CrashArtifactCollectionRequest {
    pub(crate) fn new(game_dir: PathBuf, exit_observed_at_ms: u64) -> Self {
        Self {
            game_dir,
            exit_observed_at_ms,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Candidate {
    path: PathBuf,
    kind: CrashArtifactKind,
    modified_at: SystemTime,
}

pub(crate) async fn collect_crash_evidence(
    request: CrashArtifactCollectionRequest,
) -> Option<CrashEvidence> {
    tokio::time::timeout(COLLECTION_DEADLINE, collect_within_deadline(request))
        .await
        .ok()
        .flatten()
}

async fn collect_within_deadline(request: CrashArtifactCollectionRequest) -> Option<CrashEvidence> {
    let mut candidates = Vec::new();
    scan_directory(
        &request.game_dir.join("crash-reports"),
        CrashArtifactKind::MinecraftCrashReport,
        request.exit_observed_at_ms,
        &mut candidates,
    )
    .await;
    scan_directory(
        &request.game_dir,
        CrashArtifactKind::JvmFatalError,
        request.exit_observed_at_ms,
        &mut candidates,
    )
    .await;

    let candidate = newest_candidate(candidates)?;
    let raw = read_stable_regular_prefix(&candidate.path, candidate.modified_at).await?;
    parse_crash_evidence(candidate.kind, &raw)
}

async fn scan_directory(
    directory: &Path,
    kind: CrashArtifactKind,
    exit_observed_at_ms: u64,
    candidates: &mut Vec<Candidate>,
) {
    if !is_regular_directory(directory).await {
        return;
    }
    let Ok(mut entries) = tokio::fs::read_dir(directory).await else {
        return;
    };
    for _ in 0..MAX_SCANNED_ENTRIES {
        let entry = match entries.next_entry().await {
            Ok(Some(entry)) => entry,
            Ok(None) | Err(_) => return,
        };
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if !artifact_name_matches(kind, &name) {
            continue;
        }
        let Ok(file_type) = entry.file_type().await else {
            continue;
        };
        if !file_type.is_file() || file_type.is_symlink() {
            continue;
        }
        let Ok(metadata) = entry.metadata().await else {
            continue;
        };
        let Ok(modified_at) = metadata.modified() else {
            continue;
        };
        if !timestamp_is_correlated(modified_at, exit_observed_at_ms) {
            continue;
        }
        candidates.push(Candidate {
            path: entry.path(),
            kind,
            modified_at,
        });
    }
}

async fn is_regular_directory(path: &Path) -> bool {
    tokio::fs::symlink_metadata(path)
        .await
        .is_ok_and(|metadata| metadata.file_type().is_dir() && !metadata.file_type().is_symlink())
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

fn timestamp_is_correlated(modified_at: SystemTime, exit_observed_at_ms: u64) -> bool {
    system_time_ms(modified_at).is_some_and(|modified_at_ms| {
        modified_at_ms.abs_diff(exit_observed_at_ms) <= CRASH_ARTIFACT_EXIT_CORRELATION_WINDOW_MS
    })
}

fn newest_candidate(candidates: Vec<Candidate>) -> Option<Candidate> {
    candidates.into_iter().max_by(|left, right| {
        left.modified_at
            .cmp(&right.modified_at)
            .then_with(|| candidate_tie_break(left).cmp(&candidate_tie_break(right)))
    })
}

fn candidate_tie_break(candidate: &Candidate) -> (u8, &Path) {
    let kind = match candidate.kind {
        CrashArtifactKind::MinecraftCrashReport => 0,
        CrashArtifactKind::JvmFatalError => 1,
    };
    (kind, candidate.path.as_path())
}

async fn read_stable_regular_prefix(
    path: &Path,
    expected_modified_at: SystemTime,
) -> Option<Vec<u8>> {
    let before = tokio::fs::symlink_metadata(path).await.ok()?;
    if !before.file_type().is_file() || before.file_type().is_symlink() {
        return None;
    }
    if before.modified().ok() != Some(expected_modified_at) {
        return None;
    }
    let file = tokio::fs::File::open(path).await.ok()?;
    let opened = file.metadata().await.ok()?;
    if !same_read_snapshot(&before, &opened) {
        return None;
    }

    let limit = u64::try_from(MAX_CRASH_ARTIFACT_BYTES)
        .ok()?
        .saturating_add(1);
    let mut raw = Vec::with_capacity(before.len().min(limit) as usize);
    file.take(limit).read_to_end(&mut raw).await.ok()?;

    let after = tokio::fs::symlink_metadata(path).await.ok()?;
    (after.file_type().is_file()
        && !after.file_type().is_symlink()
        && same_read_snapshot(&before, &after))
    .then_some(raw)
}

fn same_read_snapshot(left: &std::fs::Metadata, right: &std::fs::Metadata) -> bool {
    left.len() == right.len() && left.modified().ok() == right.modified().ok()
}

fn system_time_ms(value: SystemTime) -> Option<u64> {
    value
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| u64::try_from(duration.as_millis()).ok())
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
        let at_10_seconds = UNIX_EPOCH + Duration::from_millis(10_000);
        let at_25_seconds = UNIX_EPOCH + Duration::from_millis(25_000);
        assert!(timestamp_is_correlated(at_10_seconds, 25_000));
        assert!(timestamp_is_correlated(at_25_seconds, 10_000));
        assert!(!timestamp_is_correlated(at_10_seconds, 25_001));
        assert!(!timestamp_is_correlated(at_25_seconds, 9_999));
    }

    #[test]
    fn newest_selection_is_deterministic_across_artifact_kinds() {
        let report = Candidate {
            path: PathBuf::from("crash-reports/crash-a.txt"),
            kind: CrashArtifactKind::MinecraftCrashReport,
            modified_at: UNIX_EPOCH + Duration::from_nanos(100),
        };
        let hs_err = Candidate {
            path: PathBuf::from("hs_err_pid1.log"),
            kind: CrashArtifactKind::JvmFatalError,
            modified_at: UNIX_EPOCH + Duration::from_nanos(101),
        };
        assert_eq!(
            newest_candidate(vec![hs_err, report]).unwrap().kind,
            CrashArtifactKind::JvmFatalError
        );
    }

    #[tokio::test]
    async fn collection_reads_one_exact_regular_artifact_and_ignores_other_files() {
        let root = TestRoot::new("regular");
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

        let evidence = collect_crash_evidence(CrashArtifactCollectionRequest::new(
            root.0.clone(),
            now_ms(),
        ))
        .await
        .expect("crash evidence");
        assert_eq!(evidence.source, CrashArtifactKind::MinecraftCrashReport);
        assert!(evidence.names_out_of_memory);
    }

    #[tokio::test]
    async fn absent_malformed_and_entry_saturated_collection_are_normal() {
        let root = TestRoot::new("absence");
        assert!(
            collect_crash_evidence(CrashArtifactCollectionRequest::new(
                root.0.clone(),
                now_ms()
            ))
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
        let evidence = collect_crash_evidence(CrashArtifactCollectionRequest::new(
            root.0.clone(),
            now_ms(),
        ))
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
            collect_crash_evidence(CrashArtifactCollectionRequest::new(
                root.0.clone(),
                now_ms()
            ))
            .await
            .is_none()
        );
    }
}
