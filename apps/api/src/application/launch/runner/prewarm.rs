use crate::state::launch_reports::LaunchProofResourceBudget;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use tokio::io::AsyncReadExt;

const PREWARM_MAX_FILES: usize = 8;
const PREWARM_MAX_TOTAL_BYTES: u64 = 2 * 1024 * 1024;
const PREWARM_MAX_FILE_BYTES: u64 = 256 * 1024;
const PREWARM_REDUCED_MAX_FILES: usize = 2;
const PREWARM_REDUCED_MAX_TOTAL_BYTES: u64 = 512 * 1024;
const PREWARM_REDUCED_MAX_FILE_BYTES: u64 = 128 * 1024;
const PREWARM_BUFFER_BYTES: usize = 16 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LaunchPrewarmBudget {
    max_files: usize,
    max_total_bytes: u64,
    max_file_bytes: u64,
}

impl Default for LaunchPrewarmBudget {
    fn default() -> Self {
        Self {
            max_files: PREWARM_MAX_FILES,
            max_total_bytes: PREWARM_MAX_TOTAL_BYTES,
            max_file_bytes: PREWARM_MAX_FILE_BYTES,
        }
    }
}

impl LaunchPrewarmBudget {
    fn reduced() -> Self {
        Self {
            max_files: PREWARM_REDUCED_MAX_FILES,
            max_total_bytes: PREWARM_REDUCED_MAX_TOTAL_BYTES,
            max_file_bytes: PREWARM_REDUCED_MAX_FILE_BYTES,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LaunchPrewarmBudgetClass {
    Default,
    Reduced,
    Skipped,
}

impl LaunchPrewarmBudgetClass {
    fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Reduced => "reduced",
            Self::Skipped => "skipped",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LaunchPrewarmSelection {
    class: LaunchPrewarmBudgetClass,
    budget: Option<LaunchPrewarmBudget>,
    reason: Option<&'static str>,
}

impl LaunchPrewarmSelection {
    fn default_budget() -> Self {
        Self {
            class: LaunchPrewarmBudgetClass::Default,
            budget: Some(LaunchPrewarmBudget::default()),
            reason: None,
        }
    }

    fn reduced(reason: &'static str) -> Self {
        Self {
            class: LaunchPrewarmBudgetClass::Reduced,
            budget: Some(LaunchPrewarmBudget::reduced()),
            reason: Some(reason),
        }
    }

    fn skipped(reason: &'static str) -> Self {
        Self {
            class: LaunchPrewarmBudgetClass::Skipped,
            budget: None,
            reason: Some(reason),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct LaunchPrewarmSummary {
    warmed_files: usize,
    warmed_bytes: u64,
    skipped_files: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct LaunchPrewarmRunSummary {
    selection: LaunchPrewarmSelection,
    warmed_files: usize,
    warmed_bytes: u64,
    skipped_files: usize,
}

pub(super) async fn prewarm_launch_plan(
    plan: &axial_launcher::VanillaLaunchPlan,
    resource_budget: Option<&LaunchProofResourceBudget>,
) -> LaunchPrewarmRunSummary {
    // Prewarm is bounded and best-effort. Resource pressure reduces or skips it.
    let selection = select_prewarm_budget(resource_budget);
    let candidate_paths = prewarm_candidate_paths(plan);
    let summary = match selection.budget {
        Some(budget) => prewarm_candidate_files(candidate_paths, budget).await,
        None => LaunchPrewarmSummary {
            skipped_files: candidate_paths.len(),
            ..LaunchPrewarmSummary::default()
        },
    };

    LaunchPrewarmRunSummary {
        selection,
        warmed_files: summary.warmed_files,
        warmed_bytes: summary.warmed_bytes,
        skipped_files: summary.skipped_files,
    }
}

fn select_prewarm_budget(
    resource_budget: Option<&LaunchProofResourceBudget>,
) -> LaunchPrewarmSelection {
    let Some(resource_budget) = resource_budget else {
        return LaunchPrewarmSelection::default_budget();
    };

    if resource_budget.cpu_pressure && resource_budget.install_pressure {
        return LaunchPrewarmSelection::skipped("cpu_and_install_pressure");
    }
    if resource_budget.disk_pressure
        && resource_budget
            .launch_disk_available_mb
            .is_some_and(|available_mb| available_mb < resource_budget.launch_disk_headroom_mb)
    {
        return LaunchPrewarmSelection::skipped("disk_headroom_pressure");
    }
    if has_prewarm_pressure(resource_budget) {
        return LaunchPrewarmSelection::reduced("resource_pressure");
    }

    LaunchPrewarmSelection::default_budget()
}

fn has_prewarm_pressure(resource_budget: &LaunchProofResourceBudget) -> bool {
    resource_budget.cpu_pressure
        || resource_budget.install_pressure
        || resource_budget.disk_pressure
        || resource_budget.active_session_count > 0
}

pub(super) fn format_prewarm_run_summary(prewarm: &LaunchPrewarmRunSummary) -> String {
    let reason = prewarm
        .selection
        .reason
        .map(|reason| format!(" reason={reason}"))
        .unwrap_or_default();
    format!(
        "Prewarmed launch data: mode={} warmed_files={} warmed_bytes={} skipped={}{}.",
        prewarm.selection.class.as_str(),
        prewarm.warmed_files,
        prewarm.warmed_bytes,
        prewarm.skipped_files,
        reason
    )
}

fn prewarm_candidate_paths(plan: &axial_launcher::VanillaLaunchPlan) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    let mut seen = HashSet::new();

    if let Some(client_jar_path) = plan.client_jar_path.as_ref() {
        push_unique_prewarm_path(&mut paths, &mut seen, client_jar_path);
    }
    for library in &plan.libraries {
        if !library.is_native && is_jar_path(&library.abs_path) {
            push_unique_prewarm_path(&mut paths, &mut seen, &library.abs_path);
        }
    }

    paths
}

fn push_unique_prewarm_path(paths: &mut Vec<PathBuf>, seen: &mut HashSet<PathBuf>, path: &Path) {
    let path = path.to_path_buf();
    if seen.insert(path.clone()) {
        paths.push(path);
    }
}

fn is_jar_path(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("jar"))
}

async fn prewarm_candidate_files<I, P>(
    candidate_paths: I,
    budget: LaunchPrewarmBudget,
) -> LaunchPrewarmSummary
where
    I: IntoIterator<Item = P>,
    P: AsRef<Path>,
{
    let mut summary = LaunchPrewarmSummary::default();
    let mut attempted_files = 0usize;

    for path in candidate_paths {
        if attempted_files >= budget.max_files || summary.warmed_bytes >= budget.max_total_bytes {
            summary.skipped_files += 1;
            continue;
        }

        let remaining_total = budget.max_total_bytes.saturating_sub(summary.warmed_bytes);
        let max_bytes = budget.max_file_bytes.min(remaining_total);
        if max_bytes == 0 {
            summary.skipped_files += 1;
            continue;
        }

        attempted_files += 1;
        match prewarm_file_prefix(path.as_ref(), max_bytes).await {
            Ok(bytes) => {
                summary.warmed_files += 1;
                summary.warmed_bytes = summary.warmed_bytes.saturating_add(bytes);
            }
            Err(_) => {
                summary.skipped_files += 1;
            }
        }
    }

    summary
}

async fn prewarm_file_prefix(path: &Path, max_bytes: u64) -> std::io::Result<u64> {
    let mut file = tokio::fs::File::open(path).await?;
    let mut warmed = 0u64;
    let mut buffer = [0u8; PREWARM_BUFFER_BYTES];

    while warmed < max_bytes {
        let remaining = max_bytes.saturating_sub(warmed);
        let limit = buffer
            .len()
            .min(usize::try_from(remaining).unwrap_or(usize::MAX));
        let read = file.read(&mut buffer[..limit]).await?;
        if read == 0 {
            break;
        }
        warmed = warmed.saturating_add(u64::try_from(read).unwrap_or(u64::MAX));
    }

    Ok(warmed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn launch_prewarm_selects_default_budget_without_pressure() {
        let budget = test_resource_budget();
        let selection = select_prewarm_budget(Some(&budget));

        assert_eq!(selection.class, LaunchPrewarmBudgetClass::Default);
        assert_eq!(selection.budget, Some(LaunchPrewarmBudget::default()));
        assert_eq!(selection.reason, None);
    }

    #[test]
    fn launch_prewarm_selects_reduced_budget_under_pressure() {
        let mut budget = test_resource_budget();
        budget.active_session_count = 1;

        let selection = select_prewarm_budget(Some(&budget));

        assert_eq!(selection.class, LaunchPrewarmBudgetClass::Reduced);
        assert_eq!(selection.budget, Some(LaunchPrewarmBudget::reduced()));
        assert_eq!(selection.reason, Some("resource_pressure"));
        let selected_budget = selection.budget.expect("reduced budget");
        assert!(selected_budget.max_files < PREWARM_MAX_FILES);
        assert!(selected_budget.max_total_bytes < PREWARM_MAX_TOTAL_BYTES);
        assert!(selected_budget.max_file_bytes < PREWARM_MAX_FILE_BYTES);
    }

    #[test]
    fn launch_prewarm_selects_skip_for_severe_pressure() {
        let mut budget = test_resource_budget();
        budget.cpu_pressure = true;
        budget.install_pressure = true;

        let selection = select_prewarm_budget(Some(&budget));

        assert_eq!(selection.class, LaunchPrewarmBudgetClass::Skipped);
        assert_eq!(selection.budget, None);
        assert_eq!(selection.reason, Some("cpu_and_install_pressure"));
    }

    #[tokio::test]
    async fn launch_prewarm_reads_bounded_prefixes_and_skips_best_effort() {
        let dir = unique_test_dir("launch-prewarm");
        fs::create_dir_all(&dir).expect("create test dir");
        let first = dir.join("first.jar");
        let second = dir.join("second.jar");
        let third = dir.join("third.jar");
        let missing = dir.join("missing.jar");
        fs::write(&first, [1u8; 10]).expect("write first");
        fs::write(&second, [2u8; 10]).expect("write second");
        fs::write(&third, [3u8; 10]).expect("write third");

        let summary = prewarm_candidate_files(
            [&first, &second, &missing, &third],
            LaunchPrewarmBudget {
                max_files: 8,
                max_total_bytes: 12,
                max_file_bytes: 8,
            },
        )
        .await;

        assert_eq!(
            summary,
            LaunchPrewarmSummary {
                warmed_files: 2,
                warmed_bytes: 12,
                skipped_files: 2,
            }
        );

        let _ = fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn launch_prewarm_caps_attempted_file_count() {
        let dir = unique_test_dir("launch-prewarm-file-cap");
        fs::create_dir_all(&dir).expect("create test dir");
        let first = dir.join("first.jar");
        let second = dir.join("second.jar");
        let third = dir.join("third.jar");
        fs::write(&first, [1u8; 10]).expect("write first");
        fs::write(&second, [2u8; 10]).expect("write second");
        fs::write(&third, [3u8; 10]).expect("write third");

        let summary = prewarm_candidate_files(
            [&first, &second, &third],
            LaunchPrewarmBudget {
                max_files: 1,
                max_total_bytes: 1024,
                max_file_bytes: 8,
            },
        )
        .await;

        assert_eq!(
            summary,
            LaunchPrewarmSummary {
                warmed_files: 1,
                warmed_bytes: 8,
                skipped_files: 2,
            }
        );

        let _ = fs::remove_dir_all(dir);
    }

    fn test_resource_budget() -> LaunchProofResourceBudget {
        LaunchProofResourceBudget {
            host_total_memory_mb: Some(16 * 1024),
            host_available_memory_mb: Some(12 * 1024),
            host_used_memory_mb: Some(4 * 1024),
            host_cpu_threads: Some(8),
            host_cpu_load_1m_x100: Some(100),
            host_cpu_load_5m_x100: Some(100),
            host_cpu_load_15m_x100: Some(100),
            launcher_process_memory_mb: Some(256),
            active_session_count: 0,
            active_install_count: 0,
            active_memory_allocation_mb: 0,
            requested_memory_mb: Some(4096),
            estimated_remaining_memory_mb: Some(12 * 1024),
            memory_headroom_mb: 2048,
            memory_pressure: false,
            cpu_pressure: false,
            install_pressure: false,
            launch_disk_available_mb: Some(16 * 1024),
            launch_disk_headroom_mb: 2048,
            disk_pressure: false,
        }
    }

    fn unique_test_dir(prefix: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time")
            .as_nanos();
        std::env::temp_dir().join(format!("{prefix}-{}-{nanos}", std::process::id()))
    }
}
