use std::fmt;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use crate::AppRootSession;

pub(crate) struct AppPathsLineage;

#[derive(Clone)]
pub struct AppPaths {
    lineage: Arc<AppPathsLineage>,
    root: PathBuf,
    config_file: PathBuf,
    instances_file: PathBuf,
    instances_dir: PathBuf,
    music_dir: PathBuf,
    library_dir: PathBuf,
    runtimes_dir: PathBuf,
    accounts_file: PathBuf,
    skins_dir: PathBuf,
    operation_journal_file: PathBuf,
    guardian_failure_memory_file: PathBuf,
    known_good_dir: PathBuf,
    persisted_state_rejection_streaks_file: PathBuf,
    performance_dir: PathBuf,
    performance_operations_dir: PathBuf,
    benchmark_suites_dir: PathBuf,
    benchmark_suite_drivers_dir: PathBuf,
    launch_reports_dir: PathBuf,
    user_mod_witness_file: PathBuf,
    update_staging_dir: PathBuf,
}

impl AppPaths {
    pub fn from_root(root: impl Into<PathBuf>) -> Result<Self, AppPathsError> {
        let root = root.into();
        validate_root(&root)?;

        Ok(Self {
            lineage: Arc::new(AppPathsLineage),
            config_file: root.join("config.json"),
            instances_file: root.join("instances.json"),
            instances_dir: root.join("instances"),
            music_dir: root.join("music"),
            library_dir: root.join("library"),
            runtimes_dir: root.join("runtimes"),
            accounts_file: root.join("accounts.json"),
            skins_dir: root.join("skins"),
            operation_journal_file: root.join("state").join("operation-journals.json"),
            guardian_failure_memory_file: root.join("guardian").join("failure-memory.json"),
            known_good_dir: root.join("state").join("known-good"),
            persisted_state_rejection_streaks_file: root
                .join("state")
                .join("persisted-state-rejection-streaks.json"),
            performance_dir: root.join("performance"),
            performance_operations_dir: root.join("performance").join("operations"),
            benchmark_suites_dir: root.join("benchmarks").join("suites"),
            benchmark_suite_drivers_dir: root.join("benchmarks").join("suite-drivers"),
            launch_reports_dir: root.join("benchmarks").join("launch"),
            user_mod_witness_file: root.join("guardian-user-mod-witnesses.json"),
            update_staging_dir: root.join("updates"),
            root,
        })
    }

    pub fn config_file(&self) -> &Path {
        &self.config_file
    }

    pub fn instances_file(&self) -> &Path {
        &self.instances_file
    }

    pub fn instances_dir(&self) -> &Path {
        &self.instances_dir
    }

    pub fn music_dir(&self) -> &Path {
        &self.music_dir
    }

    pub fn library_dir(&self) -> &Path {
        &self.library_dir
    }

    pub fn runtimes_dir(&self) -> &Path {
        &self.runtimes_dir
    }

    pub fn accounts_file(&self) -> &Path {
        &self.accounts_file
    }

    pub fn skins_dir(&self) -> &Path {
        &self.skins_dir
    }

    pub fn operation_journal_file(&self) -> &Path {
        &self.operation_journal_file
    }

    pub fn guardian_failure_memory_file(&self) -> &Path {
        &self.guardian_failure_memory_file
    }

    pub fn known_good_dir(&self) -> &Path {
        &self.known_good_dir
    }

    pub fn persisted_state_rejection_streaks_file(&self) -> &Path {
        &self.persisted_state_rejection_streaks_file
    }

    pub fn performance_dir(&self) -> &Path {
        &self.performance_dir
    }

    pub fn performance_operations_dir(&self) -> &Path {
        &self.performance_operations_dir
    }

    pub fn benchmark_suites_dir(&self) -> &Path {
        &self.benchmark_suites_dir
    }

    pub fn benchmark_suite_drivers_dir(&self) -> &Path {
        &self.benchmark_suite_drivers_dir
    }

    pub fn launch_reports_dir(&self) -> &Path {
        &self.launch_reports_dir
    }

    pub fn user_mod_witness_file(&self) -> &Path {
        &self.user_mod_witness_file
    }

    pub fn update_staging_dir(&self) -> &Path {
        &self.update_staging_dir
    }

    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    pub(crate) fn lineage(&self) -> &Arc<AppPathsLineage> {
        &self.lineage
    }

    pub fn open_root_session(&self) -> std::io::Result<AppRootSession> {
        AppRootSession::open(self)
    }
}

impl fmt::Debug for AppPaths {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("AppPaths").finish_non_exhaustive()
    }
}

impl PartialEq for AppPaths {
    fn eq(&self, other: &Self) -> bool {
        self.root == other.root
    }
}

impl Eq for AppPaths {}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum AppPathsError {
    #[error("app data root must not be empty")]
    Empty,
    #[error("app data root must be absolute")]
    NotAbsolute,
    #[error("app data root must identify a directory below the filesystem root")]
    FilesystemRoot,
    #[error("app data root contains a parent traversal")]
    ParentTraversal,
    #[error("app data root must use a normalized lexical form")]
    NonCanonicalLexical,
    #[error("app data root contains a null character")]
    NullCharacter,
    #[cfg(windows)]
    #[error("app data root uses an unsupported Windows path prefix")]
    UnsupportedWindowsPrefix,
}

fn validate_root(root: &Path) -> Result<(), AppPathsError> {
    if root.as_os_str().is_empty() {
        return Err(AppPathsError::Empty);
    }
    if contains_null(root) {
        return Err(AppPathsError::NullCharacter);
    }
    if !root.is_absolute() {
        return Err(AppPathsError::NotAbsolute);
    }
    if root
        .components()
        .any(|component| component == Component::ParentDir)
    {
        return Err(AppPathsError::ParentTraversal);
    }
    let normalized = root.components().collect::<PathBuf>();
    if normalized.as_os_str() != root.as_os_str() {
        return Err(AppPathsError::NonCanonicalLexical);
    }
    if !root
        .components()
        .any(|component| matches!(component, Component::Normal(_)))
    {
        return Err(AppPathsError::FilesystemRoot);
    }
    reject_unsupported_windows_prefix(root)?;
    Ok(())
}

#[cfg(unix)]
fn contains_null(path: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;

    path.as_os_str().as_bytes().contains(&0)
}

#[cfg(windows)]
fn contains_null(path: &Path) -> bool {
    use std::os::windows::ffi::OsStrExt;

    path.as_os_str().encode_wide().any(|unit| unit == 0)
}

#[cfg(not(any(unix, windows)))]
fn contains_null(path: &Path) -> bool {
    path.to_string_lossy().contains('\0')
}

#[cfg(windows)]
fn reject_unsupported_windows_prefix(root: &Path) -> Result<(), AppPathsError> {
    use std::path::Prefix;

    let Some(Component::Prefix(prefix)) = root.components().next() else {
        return Err(AppPathsError::UnsupportedWindowsPrefix);
    };
    if matches!(
        prefix.kind(),
        Prefix::Disk(_)
            | Prefix::UNC(_, _)
            | Prefix::VerbatimDisk(_)
            | Prefix::VerbatimUNC(_, _)
    ) {
        Ok(())
    } else {
        Err(AppPathsError::UnsupportedWindowsPrefix)
    }
}

#[cfg(not(windows))]
fn reject_unsupported_windows_prefix(_root: &Path) -> Result<(), AppPathsError> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn absolute_root(name: &str) -> PathBuf {
        std::env::temp_dir().join("axial-app-paths").join(name)
    }

    #[test]
    fn derives_every_managed_child_from_one_root() {
        let root = absolute_root("derived");
        let paths = AppPaths::from_root(root.clone()).expect("absolute root");

        assert_eq!(paths.config_file(), root.join("config.json"));
        assert_eq!(paths.instances_file(), root.join("instances.json"));
        assert_eq!(paths.instances_dir(), root.join("instances"));
        assert_eq!(paths.music_dir(), root.join("music"));
        assert_eq!(paths.library_dir(), root.join("library"));
        assert_eq!(paths.runtimes_dir(), root.join("runtimes"));
        assert_eq!(paths.accounts_file(), root.join("accounts.json"));
        assert_eq!(paths.skins_dir(), root.join("skins"));
        assert_eq!(
            paths.operation_journal_file(),
            root.join("state").join("operation-journals.json")
        );
        assert_eq!(
            paths.guardian_failure_memory_file(),
            root.join("guardian").join("failure-memory.json")
        );
        assert_eq!(paths.known_good_dir(), root.join("state").join("known-good"));
        assert_eq!(
            paths.persisted_state_rejection_streaks_file(),
            root.join("state")
                .join("persisted-state-rejection-streaks.json")
        );
        assert_eq!(paths.performance_dir(), root.join("performance"));
        assert_eq!(
            paths.performance_operations_dir(),
            root.join("performance").join("operations")
        );
        assert_eq!(
            paths.benchmark_suites_dir(),
            root.join("benchmarks").join("suites")
        );
        assert_eq!(
            paths.benchmark_suite_drivers_dir(),
            root.join("benchmarks").join("suite-drivers")
        );
        assert_eq!(
            paths.launch_reports_dir(),
            root.join("benchmarks").join("launch")
        );
        assert_eq!(
            paths.user_mod_witness_file(),
            root.join("guardian-user-mod-witnesses.json")
        );
        assert_eq!(paths.update_staging_dir(), root.join("updates"));
    }

    #[test]
    fn rejects_unsafe_roots_without_echoing_them() {
        let private_root = absolute_root("private");
        for (root, expected) in [
            (PathBuf::new(), AppPathsError::Empty),
            (PathBuf::from("relative"), AppPathsError::NotAbsolute),
            (
                private_root.join("..").join("escaped"),
                AppPathsError::ParentTraversal,
            ),
            (
                private_root.join(".").join("nested"),
                AppPathsError::NonCanonicalLexical,
            ),
            (
                absolute_root("private\0suffix"),
                AppPathsError::NullCharacter,
            ),
        ] {
            let error = AppPaths::from_root(root).expect_err("root must reject");
            assert_eq!(error, expected);
            assert!(!error.to_string().contains("private"));
        }
    }

    #[cfg(unix)]
    #[test]
    fn accepts_non_unicode_roots_without_lossy_validation() {
        use std::ffi::OsString;
        use std::os::unix::ffi::{OsStrExt, OsStringExt};

        let mut bytes = absolute_root("non-unicode").as_os_str().as_bytes().to_vec();
        bytes.extend_from_slice(&[b'/', 0xff]);
        let root = PathBuf::from(OsString::from_vec(bytes));

        AppPaths::from_root(root).expect("non-Unicode absolute root");
    }

    #[cfg(unix)]
    #[test]
    fn rejects_the_filesystem_root() {
        assert_eq!(
            AppPaths::from_root("/").expect_err("filesystem root must reject"),
            AppPathsError::FilesystemRoot
        );
    }

    #[cfg(windows)]
    #[test]
    fn rejects_the_filesystem_root() {
        assert_eq!(
            AppPaths::from_root(r"C:\").expect_err("filesystem root must reject"),
            AppPathsError::FilesystemRoot
        );
    }

    #[cfg(windows)]
    #[test]
    fn accepts_disk_and_unc_roots_in_normal_and_extended_length_forms() {
        for root in [
            r"C:\Axial\Data",
            r"\\server\share\Axial",
            r"\\?\C:\Axial\Data",
            r"\\?\UNC\server\share\Axial",
        ] {
            AppPaths::from_root(root).expect("supported Windows app root");
        }
    }

    #[cfg(windows)]
    #[test]
    fn rejects_device_and_opaque_verbatim_namespaces() {
        for root in [
            r"\\.\PIPE\Axial",
            r"\\?\GLOBALROOT\Device\HarddiskVolume1\Axial",
        ] {
            assert_eq!(
                AppPaths::from_root(root).expect_err("unsupported Windows namespace"),
                AppPathsError::UnsupportedWindowsPrefix
            );
        }
    }

    #[test]
    fn debug_output_does_not_expose_the_root() {
        let paths = AppPaths::from_root(absolute_root("debug-private")).expect("absolute root");
        assert_eq!(format!("{paths:?}"), "AppPaths { .. }");
    }
}
