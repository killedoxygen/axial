use crate::app::{
    ApplicationStartupError, ApplicationStartupHealth, start_application_background_workflows,
};
use crate::state::{AppShutdownStep, AppState, AppStateInit, InstallStore, SessionStore};
use axial_config::{
    AppPaths, AppRootSession, ConfigStore, ConfigStoreError, InstanceStore, InstanceStoreError,
};
use axial_performance::{InstallError, PerformanceManager};
use axial_resource::{
    PhysicalIoClass, PhysicalWorkError, PhysicalWorkRequest, process_physical_work,
};
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;

pub const APP_IDENTIFIER: &str = "dev.mateoltd.axial";
pub const DEVELOPMENT_APP_IDENTIFIER: &str = "dev.mateoltd.axial.dev";
pub const APP_ROOT_MODE_ENV: &str = "AXIAL_APP_ROOT_MODE";
pub const APP_ROOT_ENV: &str = "AXIAL_APP_ROOT";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExplicitAppRootPurpose {
    Test,
    Portable,
}

#[derive(Clone, Eq, PartialEq)]
pub enum AppRootSelection {
    Production,
    Development,
    Explicit {
        purpose: ExplicitAppRootPurpose,
        root: PathBuf,
    },
}

impl AppRootSelection {
    pub fn test(root: impl Into<PathBuf>) -> Self {
        Self::Explicit {
            purpose: ExplicitAppRootPurpose::Test,
            root: root.into(),
        }
    }

    pub fn portable(root: impl Into<PathBuf>) -> Self {
        Self::Explicit {
            purpose: ExplicitAppRootPurpose::Portable,
            root: root.into(),
        }
    }
}

impl fmt::Debug for AppRootSelection {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Production => formatter.write_str("Production"),
            Self::Development => formatter.write_str("Development"),
            Self::Explicit { purpose, .. } => formatter
                .debug_struct("Explicit")
                .field("purpose", purpose)
                .finish_non_exhaustive(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum AppRootError {
    #[error("platform app data directory is unavailable")]
    PlatformDirectoryUnavailable,
    #[error("platform app data directory is invalid")]
    PlatformDirectoryInvalid,
    #[error("app data root mode is invalid")]
    InvalidMode,
    #[error("portable app data root is required")]
    PortableRootRequired,
    #[error("app data root is not valid for the selected mode")]
    UnexpectedExplicitRoot,
    #[error("explicit app data root is invalid")]
    ExplicitRootInvalid,
    #[error("desktop app root mode does not match the native application identity")]
    NativeIdentifierMismatch,
}

pub struct ApplicationLoadRequest {
    pub root: AppRootSelection,
    pub app_name: String,
    pub version: String,
}

pub struct LoadedApplication {
    pub state: AppState,
    pub health: ApplicationStartupHealth,
}

#[derive(Debug, thiserror::Error)]
pub enum ApplicationLoadError {
    #[error(transparent)]
    RootSelection(#[from] AppRootError),
    #[error("failed to open the application root: {0}")]
    Root(std::io::Error),
    #[error("config startup work stopped: {0}")]
    ConfigTask(PhysicalWorkError),
    #[error("config startup failed: {0}")]
    Config(#[source] ConfigStoreError),
    #[error("instance-registry startup work stopped: {0}")]
    InstancesTask(PhysicalWorkError),
    #[error("instance-registry startup failed: {0}")]
    Instances(#[source] InstanceStoreError),
    #[error("performance startup work stopped: {0}")]
    PerformanceTask(PhysicalWorkError),
    #[error("performance startup failed: {0}")]
    Performance(#[source] InstallError),
    #[error("application State startup failed: {0}")]
    State(std::io::Error),
    #[error("application startup failed at {source}; shutdown step: {shutdown_step:?}")]
    Startup {
        source: ApplicationStartupError,
        shutdown_step: Option<AppShutdownStep>,
    },
}

pub async fn load_application(
    request: ApplicationLoadRequest,
) -> Result<LoadedApplication, ApplicationLoadError> {
    let paths = resolve_app_paths(request.root)?;
    let root_session = Arc::new(open_app_root_session(&paths).map_err(ApplicationLoadError::Root)?);
    let config_paths = paths.clone();
    let config_root = Arc::clone(&root_session);
    let config = run_application_startup_work(PhysicalIoClass::Write, move || {
        ConfigStore::load_for_startup(config_paths, config_root)
    })
    .await
    .map_err(ApplicationLoadError::ConfigTask)?
    .map_err(ApplicationLoadError::Config)?;
    let instance_paths = paths.clone();
    let instance_root = Arc::clone(&root_session);
    let instances = run_application_startup_work(PhysicalIoClass::Write, move || {
        InstanceStore::load_for_startup(instance_paths, instance_root)
    })
    .await
    .map_err(ApplicationLoadError::InstancesTask)?
    .map_err(ApplicationLoadError::Instances)?;
    let mut startup_warnings = config.warnings;
    startup_warnings.extend(instances.warnings);
    drop(
        root_session
            .prepare_performance_directory()
            .map_err(ApplicationLoadError::Root)?,
    );
    let performance_dir = paths.performance_dir().to_path_buf();
    let performance = run_application_startup_work(PhysicalIoClass::Heavy, move || {
        PerformanceManager::load_for_startup(&performance_dir)
    })
    .await
    .map_err(ApplicationLoadError::PerformanceTask)?
    .map_err(ApplicationLoadError::Performance)?;
    let state = AppState::load(AppStateInit {
        app_name: request.app_name,
        version: request.version,
        config: Arc::new(config.store),
        instances: Arc::new(instances.store),
        installs: Arc::new(InstallStore::new()),
        sessions: Arc::new(SessionStore::new()),
        performance: Arc::new(performance),
        startup_warnings,
    })
    .await
    .map_err(ApplicationLoadError::State)?;
    let health = match start_application_background_workflows(&state).await {
        Ok(health) => health,
        Err(source) => {
            let shutdown_step = state.shutdown().await.err().map(|error| error.step());
            return Err(ApplicationLoadError::Startup {
                source,
                shutdown_step,
            });
        }
    };
    Ok(LoadedApplication { state, health })
}

async fn run_application_startup_work<T, Work>(
    io_class: PhysicalIoClass,
    work: Work,
) -> Result<T, PhysicalWorkError>
where
    T: Send + 'static,
    Work: FnOnce() -> T + Send + 'static,
{
    process_physical_work()
        .admit(PhysicalWorkRequest::foreground(io_class, 0))
        .await?
        .run(move |_| work())
        .await
}

pub fn app_root_selection_from_environment() -> Result<AppRootSelection, AppRootError> {
    parse_app_root_selection(
        std::env::var_os(APP_ROOT_MODE_ENV),
        std::env::var_os(APP_ROOT_ENV),
    )
}

pub fn desktop_app_root_selection_from_environment(
    native_identifier: &str,
) -> Result<AppRootSelection, AppRootError> {
    parse_desktop_app_root_selection(
        native_identifier,
        std::env::var_os(APP_ROOT_MODE_ENV),
        std::env::var_os(APP_ROOT_ENV),
    )
}

pub fn resolve_app_paths(selection: AppRootSelection) -> Result<AppPaths, AppRootError> {
    match selection {
        AppRootSelection::Production | AppRootSelection::Development => {
            reject_malformed_platform_environment()?;
            resolve_app_paths_with_local_data(selection, dirs::data_local_dir())
        }
        AppRootSelection::Explicit { root, .. } => {
            AppPaths::from_root(root).map_err(|_| AppRootError::ExplicitRootInvalid)
        }
    }
}

pub fn open_app_root_session(paths: &AppPaths) -> Result<AppRootSession, std::io::Error> {
    paths.open_root_session_with_state_successor(crate::state::admit_startup_state_successor)
}

fn parse_app_root_selection(
    mode: Option<OsString>,
    explicit_root: Option<OsString>,
) -> Result<AppRootSelection, AppRootError> {
    match (mode.as_deref(), explicit_root) {
        (None, None) => Ok(AppRootSelection::Production),
        (None, Some(_)) => Err(AppRootError::InvalidMode),
        (Some(value), None) if value == OsStr::new("production") => {
            Ok(AppRootSelection::Production)
        }
        (Some(value), None) if value == OsStr::new("development") => {
            Ok(AppRootSelection::Development)
        }
        (Some(value), Some(root)) if value == OsStr::new("portable") => {
            Ok(AppRootSelection::portable(PathBuf::from(root)))
        }
        (Some(value), None) if value == OsStr::new("portable") => {
            Err(AppRootError::PortableRootRequired)
        }
        (Some(value), Some(_))
            if value == OsStr::new("production") || value == OsStr::new("development") =>
        {
            Err(AppRootError::UnexpectedExplicitRoot)
        }
        _ => Err(AppRootError::InvalidMode),
    }
}

fn parse_desktop_app_root_selection(
    native_identifier: &str,
    mode: Option<OsString>,
    explicit_root: Option<OsString>,
) -> Result<AppRootSelection, AppRootError> {
    if mode.is_none() && explicit_root.is_none() {
        return native_app_root_selection(native_identifier);
    }

    let requested = parse_app_root_selection(mode, explicit_root)?;
    if matches!(
        &requested,
        AppRootSelection::Explicit {
            purpose: ExplicitAppRootPurpose::Portable,
            ..
        }
    ) {
        return Ok(requested);
    }

    let native_selection = native_app_root_selection(native_identifier)?;
    if native_selection == requested {
        Ok(requested)
    } else {
        Err(AppRootError::NativeIdentifierMismatch)
    }
}

fn native_app_root_selection(native_identifier: &str) -> Result<AppRootSelection, AppRootError> {
    match native_identifier {
        APP_IDENTIFIER => Ok(AppRootSelection::Production),
        DEVELOPMENT_APP_IDENTIFIER => Ok(AppRootSelection::Development),
        _ => Err(AppRootError::NativeIdentifierMismatch),
    }
}

fn resolve_app_paths_with_local_data(
    selection: AppRootSelection,
    local_data: Option<PathBuf>,
) -> Result<AppPaths, AppRootError> {
    let identifier = match selection {
        AppRootSelection::Production => APP_IDENTIFIER,
        AppRootSelection::Development => DEVELOPMENT_APP_IDENTIFIER,
        AppRootSelection::Explicit { root, .. } => {
            return AppPaths::from_root(root).map_err(|_| AppRootError::ExplicitRootInvalid);
        }
    };
    let local_data = local_data.ok_or(AppRootError::PlatformDirectoryUnavailable)?;
    AppPaths::from_root(local_data.join(identifier))
        .map_err(|_| AppRootError::PlatformDirectoryInvalid)
}

#[cfg(all(unix, not(target_os = "macos")))]
fn reject_malformed_platform_environment() -> Result<(), AppRootError> {
    let Some(value) = std::env::var_os("XDG_DATA_HOME") else {
        return Ok(());
    };
    if value.is_empty() || std::path::Path::new(&value).is_absolute() {
        Ok(())
    } else {
        Err(AppRootError::PlatformDirectoryInvalid)
    }
}

#[cfg(not(all(unix, not(target_os = "macos"))))]
fn reject_malformed_platform_environment() -> Result<(), AppRootError> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn absolute_root(name: &str) -> PathBuf {
        std::env::temp_dir().join("axial-bootstrap").join(name)
    }

    #[test]
    fn production_and_development_use_distinct_tauri_compatible_identifiers() {
        let local_data = absolute_root("local-data");
        let production = resolve_app_paths_with_local_data(
            AppRootSelection::Production,
            Some(local_data.clone()),
        )
        .expect("production paths");
        let development = resolve_app_paths_with_local_data(
            AppRootSelection::Development,
            Some(local_data.clone()),
        )
        .expect("development paths");

        assert_eq!(
            production.config_file(),
            local_data.join(APP_IDENTIFIER).join("config.json")
        );
        assert_eq!(
            development.config_file(),
            local_data
                .join(DEVELOPMENT_APP_IDENTIFIER)
                .join("config.json")
        );
        assert_ne!(production, development);
    }

    #[test]
    fn p01_b01_contract_cross_owner() {
        let root = absolute_root("cross-owner");
        let paths = resolve_app_paths_with_local_data(AppRootSelection::test(root.clone()), None)
            .expect("explicit test root");
        assert_eq!(paths.config_file(), root.join("config.json"));
        assert_eq!(paths.instances_file(), root.join("instances.json"));
        assert_eq!(paths.performance_dir(), root.join("performance"));
        assert_eq!(paths.update_staging_dir(), root.join("updates"));
        assert_eq!(APP_IDENTIFIER, "dev.mateoltd.axial");
        assert_eq!(DEVELOPMENT_APP_IDENTIFIER, "dev.mateoltd.axial.dev");

        let api_main = include_str!("main.rs");
        let desktop_main = include_str!("../../desktop/src/main.rs");
        for (source, selection) in [
            (api_main, "root: app_root_selection_from_environment()?"),
            (
                desktop_main,
                "root: desktop_app_root_selection_from_environment(",
            ),
        ] {
            assert_eq!(
                source
                    .matches("load_application(ApplicationLoadRequest")
                    .count(),
                1
            );
            assert_eq!(source.matches(selection).count(), 1);
            assert!(!source.contains("ConfigStore::load_for_startup"));
            assert!(!source.contains("InstanceStore::load_for_startup"));
            assert!(!source.contains("PerformanceManager::load_for_startup"));
            assert!(!source.contains(&["paths", ".root()"].concat()));
        }
        let bootstrap = include_str!("bootstrap.rs");
        assert!(
            bootstrap
                .find("resolve_app_paths(request.root)")
                .expect("root resolution")
                < bootstrap
                    .find("ConfigStore::load_for_startup")
                    .expect("first store construction")
        );
        assert!(bootstrap.contains("paths.performance_dir()"));
        assert!(desktop_main.contains("context.config().identifier.as_str()"));
    }

    #[tokio::test]
    async fn p02_b07_contract_typed_loader_gates_serving_and_joins_owned_work() {
        let root = absolute_root("p02-b07-typed-loader");
        let _ = std::fs::remove_dir_all(&root);
        let loaded = load_application(ApplicationLoadRequest {
            root: AppRootSelection::test(root.clone()),
            app_name: "Axial".to_string(),
            version: "test".to_string(),
        })
        .await
        .expect("typed application load");

        assert_eq!(
            loaded.health.performance_rules,
            crate::app::OptionalWorkflowState::Disabled
        );
        assert_ne!(
            loaded.health.idle_integrity,
            crate::app::OptionalWorkflowState::AdmissionFailed
        );
        loaded.state.quiesce().await.expect("quiesce application");
        assert_eq!(loaded.state.active_producer_count(), 0);
        assert_eq!(
            start_application_background_workflows(&loaded.state)
                .await
                .expect_err("mandatory settlement must reject after quiescence"),
            ApplicationStartupError::GuardianFailureMemory
        );
        loaded.state.shutdown().await.expect("shutdown application");
        assert_eq!(loaded.state.active_persistence_worker_count(), 0);
        drop(loaded);
        std::fs::remove_dir_all(root).expect("remove typed loader fixture");
    }

    #[tokio::test]
    async fn p02_b07_contract_cross_owner_refuses_mismatched_request_handoff() {
        let first_root = absolute_root("p02-b07-first-state");
        let second_root = absolute_root("p02-b07-second-state");
        let _ = std::fs::remove_dir_all(&first_root);
        let _ = std::fs::remove_dir_all(&second_root);
        let first = load_application(ApplicationLoadRequest {
            root: AppRootSelection::test(first_root.clone()),
            app_name: "Axial".to_string(),
            version: "test".to_string(),
        })
        .await
        .expect("load first application");
        let second = load_application(ApplicationLoadRequest {
            root: AppRootSelection::test(second_root.clone()),
            app_name: "Axial".to_string(),
            version: "test".to_string(),
        })
        .await
        .expect("load second application");
        let request = first
            .state
            .try_admit_request()
            .expect("admit first request");
        let handoff = request.producer_handoff();

        assert!(second.state.try_claim_request_producer(&handoff).is_err());
        let producer = first
            .state
            .try_claim_request_producer(&handoff)
            .expect("matching State claims request producer");
        drop(producer);
        drop(request);
        first.state.shutdown().await.expect("shutdown first State");
        second
            .state
            .shutdown()
            .await
            .expect("shutdown second State");
        assert_eq!(first.state.active_producer_count(), 0);
        assert_eq!(second.state.active_producer_count(), 0);
        drop(first);
        drop(second);
        std::fs::remove_dir_all(first_root).expect("remove first fixture");
        std::fs::remove_dir_all(second_root).expect("remove second fixture");
    }

    #[test]
    fn explicit_roots_are_injected_without_a_platform_or_cwd_fallback() {
        let root = absolute_root("explicit");
        for selection in [
            AppRootSelection::test(root.clone()),
            AppRootSelection::portable(root.clone()),
        ] {
            let paths = resolve_app_paths_with_local_data(selection, None)
                .expect("explicit paths do not need platform data");
            assert_eq!(paths.config_file(), root.join("config.json"));
        }
    }

    #[test]
    fn unavailable_or_relative_platform_data_fails_without_a_relative_fallback() {
        assert_eq!(
            resolve_app_paths_with_local_data(AppRootSelection::Production, None),
            Err(AppRootError::PlatformDirectoryUnavailable)
        );
        assert_eq!(
            resolve_app_paths_with_local_data(
                AppRootSelection::Development,
                Some(PathBuf::from("relative")),
            ),
            Err(AppRootError::PlatformDirectoryInvalid)
        );
    }

    #[test]
    fn environment_selection_is_closed_and_path_free() {
        assert_eq!(
            parse_app_root_selection(None, None),
            Ok(AppRootSelection::Production)
        );
        assert_eq!(
            parse_app_root_selection(Some(OsString::from("development")), None),
            Ok(AppRootSelection::Development)
        );
        assert_eq!(
            parse_app_root_selection(
                Some(OsString::from("portable")),
                Some(absolute_root("portable-private").into_os_string()),
            ),
            Ok(AppRootSelection::portable(absolute_root(
                "portable-private"
            )))
        );

        for (mode, root, expected) in [
            (
                Some(OsString::from("unknown-private")),
                None,
                AppRootError::InvalidMode,
            ),
            (
                Some(OsString::from("portable")),
                None,
                AppRootError::PortableRootRequired,
            ),
            (
                Some(OsString::from("development")),
                Some(OsString::from("private-root")),
                AppRootError::UnexpectedExplicitRoot,
            ),
        ] {
            let error = parse_app_root_selection(mode, root).expect_err("selection must reject");
            assert_eq!(error, expected);
            assert!(!error.to_string().contains("private"));
        }
    }

    #[test]
    fn desktop_without_environment_uses_the_generated_native_identity() {
        assert_eq!(
            parse_desktop_app_root_selection(APP_IDENTIFIER, None, None),
            Ok(AppRootSelection::Production)
        );
        assert_eq!(
            parse_desktop_app_root_selection(DEVELOPMENT_APP_IDENTIFIER, None, None),
            Ok(AppRootSelection::Development)
        );
    }

    #[test]
    fn desktop_explicit_modes_must_match_the_generated_native_identity() {
        assert_eq!(
            parse_desktop_app_root_selection(
                APP_IDENTIFIER,
                Some(OsString::from("production")),
                None,
            ),
            Ok(AppRootSelection::Production)
        );
        assert_eq!(
            parse_desktop_app_root_selection(
                DEVELOPMENT_APP_IDENTIFIER,
                Some(OsString::from("development")),
                None,
            ),
            Ok(AppRootSelection::Development)
        );

        for (identifier, mode) in [
            (APP_IDENTIFIER, "development"),
            (DEVELOPMENT_APP_IDENTIFIER, "production"),
        ] {
            let error =
                parse_desktop_app_root_selection(identifier, Some(OsString::from(mode)), None)
                    .expect_err("mismatched desktop mode must reject");
            assert_eq!(error, AppRootError::NativeIdentifierMismatch);
            assert!(!error.to_string().contains(identifier));
        }
    }

    #[test]
    fn desktop_portable_root_bypasses_native_identity_without_weakening_validation() {
        let root = absolute_root("desktop-portable");
        assert_eq!(
            parse_desktop_app_root_selection(
                "unrecognized.native.identity",
                Some(OsString::from("portable")),
                Some(root.clone().into_os_string()),
            ),
            Ok(AppRootSelection::portable(root))
        );
        assert_eq!(
            parse_desktop_app_root_selection(
                "unrecognized.native.identity",
                Some(OsString::from("portable")),
                None,
            ),
            Err(AppRootError::PortableRootRequired)
        );
        assert_eq!(
            parse_desktop_app_root_selection("unrecognized.native.identity", None, None),
            Err(AppRootError::NativeIdentifierMismatch)
        );
    }

    #[test]
    fn explicit_relative_root_fails_without_echoing_it() {
        let error = resolve_app_paths(AppRootSelection::portable("private-relative"))
            .expect_err("relative root must reject");
        assert_eq!(error, AppRootError::ExplicitRootInvalid);
        assert!(!error.to_string().contains("private-relative"));
    }
}
