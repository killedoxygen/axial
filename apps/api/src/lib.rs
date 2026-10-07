//! Composition for the isolated replacement application and its local HTTP API.

pub mod events;
#[cfg(feature = "embedded-frontend")]
mod frontend;
#[cfg(test)]
mod frontend_build_support;
#[cfg(test)]
mod library_tests;
#[cfg(all(test, unix))]
mod offline_journey_tests;
mod open_folder;
#[cfg(test)]
mod reset_tests;
pub mod routes;
pub mod transport;

use axial_app::{
    accounts::{
        credential_store::CredentialStore,
        directory::{AccountDirectory, MIGRATION as ACCOUNTS_MIGRATION},
        session::AuthService,
    },
    catalog::Catalog,
    content::{catalog::ContentService, install::ContentMutations},
    install::queue::InstallQueue,
    instances::{
        create::InstanceService,
        directory::{InstanceDirectories, Registry},
        setup::SetupService,
    },
    launch::{
        coordinator::{LaunchCoordinator, LaunchIntents},
        reports::LaunchReportStore,
        session::SessionManager,
    },
    library::{LibraryId, LibraryLifecycle, LibraryOpenOutcome},
    music::MusicService,
    network::{ClientConfig, ProviderClient},
    performance::{PerformanceService, benchmarks::BenchmarkService},
    public::DEVELOPMENT_APPLICATION_ID,
    resources::{ResourceService, folders::FolderService},
    runtime::discovery::RuntimeDiscovery,
    settings::{SETTINGS_MIGRATION, SettingsStore},
    skins::{ProfileMedia, library::SavedSkinLibrary, store::SavedSkinStore},
    storage::MetadataStore,
    tasks::{Exclusions, TaskOwner},
    telemetry::{CollectorConfig, Telemetry, TelemetryEnvironment, TelemetryEvent},
    update::UpdateService,
};
use axial_minecraft::ManagedRuntimeCache;
use axum::Router;
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, OpenOptions},
    io::{self, Read, Write},
    path::{Component, Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::{
    net::TcpListener,
    sync::{Mutex, watch},
    task::{JoinHandle, JoinSet},
};
use transport::{ApiTransportBootstrap, LocalApiAuthority};

const PROFILE_MARKER: &str = ".axial-rewrite-profile";
const DOMAIN_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(30);
const HTTP_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(3);

/// A failed root acquisition can still own native filesystem obligations.
/// The outer shell must keep this value until `try_preserve` returns `Ok`.
pub struct StartupError {
    message: String,
    root: Option<LibraryOpenOutcome>,
    runtime: Option<ManagedRuntimeCache>,
    interrupted_reset: bool,
}

impl StartupError {
    fn with_library(message: String, library: LibraryLifecycle) -> Self {
        Self {
            message,
            root: Some(LibraryOpenOutcome::Ready(library)),
            runtime: None,
            interrupted_reset: false,
        }
    }

    pub fn interrupted_reset(&self) -> bool {
        self.interrupted_reset
    }

    /// Only a fresh native confirmation may consume this retained root.
    pub fn take_interrupted_reset_session(&mut self) -> Result<axial_fs::RootSession, String> {
        if !self.interrupted_reset {
            return Err("This startup failure does not authorize a reset.".into());
        }
        let Some(LibraryOpenOutcome::Ready(library)) = self.root.as_ref() else {
            return Err("The interrupted profile is not available for reset.".into());
        };
        library.close_admission();
        let session = library.take_reset_session().map_err(|_| {
            "The profile is still in use. Its files have been preserved.".to_owned()
        })?;
        self.root.take();
        self.interrupted_reset = false;
        Ok(session)
    }

    pub fn try_preserve(mut self) -> Result<String, Self> {
        if self
            .runtime
            .as_ref()
            .is_some_and(|runtime| runtime.settle().is_err())
        {
            return Err(self);
        }
        self.runtime.take();
        match self.root.take() {
            None => Ok(self.message),
            Some(LibraryOpenOutcome::Unresolved(obligation)) => {
                match obligation.acknowledge_preserved() {
                    Ok(()) => Ok(self.message),
                    Err(obligation) => {
                        self.root = Some(LibraryOpenOutcome::Unresolved(obligation));
                        Err(self)
                    }
                }
            }
            Some(LibraryOpenOutcome::Ready(library)) => match library.try_preserve() {
                Ok(()) => Ok(self.message),
                Err(_) => {
                    self.root = Some(LibraryOpenOutcome::Ready(library));
                    Err(self)
                }
            },
            Some(outcome) => {
                self.root = Some(outcome);
                Err(self)
            }
        }
    }
}

impl From<String> for StartupError {
    fn from(message: String) -> Self {
        Self {
            message,
            root: None,
            runtime: None,
            interrupted_reset: false,
        }
    }
}
impl From<&str> for StartupError {
    fn from(message: &str) -> Self {
        message.to_owned().into()
    }
}
impl std::fmt::Display for StartupError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}
impl std::fmt::Debug for StartupError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("StartupError")
            .field("message", &self.message)
            .field("retains_root", &self.root.is_some())
            .finish()
    }
}
impl std::error::Error for StartupError {}

/// Explicit shell dependencies. Features receive their own narrow dependencies.
pub struct DesktopServices {
    pub server: Arc<ServerHandle>,
    pub tasks: TaskOwner,
    pub profile_root: PathBuf,
    pub library: LibraryLifecycle,
    pub settings: Arc<SettingsStore>,
    pub accounts: Arc<AccountDirectory>,
    pub auth: Arc<AuthService>,
    pub telemetry: Arc<Telemetry>,
    pub updates: UpdateService,
    pub catalog: Arc<Catalog>,
    pub instances: Arc<InstanceService>,
    pub installs: Arc<InstallQueue>,
    pub sessions: SessionManager,
    pub skins: Arc<ProfileMedia>,
    pub performance: PerformanceService,
    pub launch: Arc<LaunchCoordinator>,
    pub benchmarks: Arc<BenchmarkService>,
    pub content_mutations: Arc<ContentMutations>,
    pub resources: Arc<ResourceService>,
    pub music: Arc<MusicService>,
}

struct ServerTask {
    join: Option<JoinHandle<io::Result<()>>>,
    result: Option<Result<(), String>>,
}

/// Owns HTTP connections until they finish, including when a caller stops waiting.
pub struct ServerHandle {
    authority: LocalApiAuthority,
    tasks: TaskOwner,
    library: LibraryLifecycle,
    instances: Arc<InstanceService>,
    setup: Arc<SetupService>,
    installs: Arc<InstallQueue>,
    sessions: SessionManager,
    skins: Arc<ProfileMedia>,
    performance: PerformanceService,
    content_mutations: Arc<ContentMutations>,
    resources: Arc<ResourceService>,
    music: Arc<MusicService>,
    runtime_cache: ManagedRuntimeCache,
    shutdown: watch::Sender<bool>,
    serving: Mutex<ServerTask>,
    telemetry_worker: Mutex<ServerTask>,
    rules_worker: Mutex<ServerTask>,
    file_settlement: Mutex<Option<JoinHandle<Result<(), String>>>>,
    shutdown_settled: AtomicBool,
}

impl ServerHandle {
    pub fn bootstrap(&self) -> ApiTransportBootstrap {
        self.authority.bootstrap()
    }

    pub fn is_shutdown_settled(&self) -> bool {
        self.shutdown_settled.load(Ordering::Acquire)
    }

    /// Destructive shell actions must check before changing terminal admission.
    pub fn ensure_no_interrupted_launch(&self) -> Result<(), String> {
        self.library.ensure_no_interrupted_launch().map_err(|_| {
            "A previous game process has not been proven settled. The local API remains available."
                .to_string()
        })
    }

    pub fn ensure_reset_allowed(&self) -> Result<(), String> {
        self.ensure_no_interrupted_launch()?;
        if self.instances.has_pending_intents() {
            return Err("Reset is blocked while instance changes still require recovery.".into());
        }
        if self.tasks.status().is_idle() && self.content_mutations.has_unsettled_effects() {
            return Err("Reset is blocked while content changes still require recovery.".into());
        }
        Ok(())
    }

    pub fn ensure_reset_settled(&self) -> Result<(), String> {
        self.ensure_reset_allowed()?;
        if self.tasks.shutdown_receipt().is_none()
            || self.content_mutations.has_unsettled_effects()
            || self.installs.has_unsettled_effects()
        {
            return Err("Reset is blocked while file changes still require recovery.".into());
        }
        Ok(())
    }

    pub fn ensure_update_allowed(&self) -> Result<(), String> {
        self.ensure_no_interrupted_launch()?;
        if self.instances.has_pending_intents()
            || self.instances.has_unsettled_effects()
            || self.performance.has_unsettled_effects()
            || self
                .performance
                .pending_count()
                .map_or(true, |count| count != 0)
            || self.content_mutations.has_unsettled_effects()
            || self.resources.has_unsettled_effects()
            || self.installs.has_unsettled_effects()
        {
            return Err("Update is blocked while file changes still require recovery.".into());
        }
        Ok(())
    }

    /// Dropping this waiter retains the actual task handle for a later caller.
    pub async fn wait(&self) -> Result<(), String> {
        Self::join_owned(&self.serving, "The local API worker did not stop cleanly.").await
    }

    async fn join_owned(task: &Mutex<ServerTask>, failure: &'static str) -> Result<(), String> {
        let mut serving = task.lock().await;
        if let Some(result) = &serving.result {
            return result.clone();
        }
        let result = match serving.join.as_mut() {
            Some(join) => match join.await {
                Ok(Ok(())) => Ok(()),
                Ok(Err(_)) | Err(_) => Err(failure.into()),
            },
            None => Err(failure.into()),
        };
        serving.join.take();
        serving.result = Some(result.clone());
        result
    }

    /// Mutations settle before HTTP bodies are closed. A timeout never aborts
    /// application work or discards the task owner's retained obligations.
    /// Historical interrupted launches stay fenced and preserved, not settled.
    pub async fn shutdown(&self) -> Result<(), String> {
        self.installs.close_admission();
        self.library.close_admission();
        self.skins.shutdown().await.map_err(|_| {
            "Profile media has not settled. The local API remains available.".to_string()
        })?;
        self.sessions
            .shutdown(DOMAIN_SHUTDOWN_TIMEOUT)
            .await
            .map_err(|_| {
                "Game processes have not settled. The local API remains available.".to_string()
            })?;
        self.tasks
            .shutdown(DOMAIN_SHUTDOWN_TIMEOUT)
            .await
            .map_err(|_| {
                "Application work has not settled. The local API remains available.".to_string()
            })?;
        if !matches!(
            tokio::time::timeout(DOMAIN_SHUTDOWN_TIMEOUT, self.installs.join_observers()).await,
            Ok(Ok(()))
        ) {
            return Err(
                "Installation status workers have not joined. The local API remains available."
                    .into(),
            );
        }
        self.settle_files().await?;
        self.installs.close_events();
        self.shutdown.send_replace(true);
        let http = self.wait().await;
        let telemetry = Self::join_owned(
            &self.telemetry_worker,
            "The telemetry worker did not stop cleanly.",
        )
        .await;
        let rules = Self::join_owned(
            &self.rules_worker,
            "The performance rules worker did not stop cleanly.",
        )
        .await;
        self.shutdown_settled.store(true, Ordering::Release);
        http.and(telemetry).and(rules)
    }

    async fn settle_files(&self) -> Result<(), String> {
        // A cancelled shell waiter leaves this blocking effect owner and its
        // handle in place. A later shutdown joins the same attempt first.
        let mut pending = self.file_settlement.lock().await;
        if pending.is_none() {
            let receipt = self.tasks.shutdown_receipt().ok_or_else(|| {
                "Application work has not joined. The local API remains available.".to_string()
            })?;
            let instances = self.instances.clone();
            let setup = self.setup.clone();
            let performance = self.performance.clone();
            let content = self.content_mutations.clone();
            let resources = self.resources.clone();
            let installs = self.installs.clone();
            let runtime = self.runtime_cache.clone();
            let music = self.music.clone();
            let library = self.library.clone();
            *pending = Some(tokio::task::spawn_blocking(move || {
                if instances.has_unsettled_effects()
                    || performance.has_unsettled_effects()
                    || resources.has_unsettled_effects()
                {
                    return Err(
                        "Filesystem changes have not settled. The local API remains available."
                            .into(),
                    );
                }
                content.release_shutdown_admissions(&receipt).map_err(|_| {
                    "Content file effects have not settled. The local API remains available."
                        .to_string()
                })?;
                setup.release_shutdown_admissions(&receipt).map_err(|_| {
                    "Instance setup effects have not settled. The local API remains available."
                        .to_string()
                })?;
                installs.preserve_shutdown(&receipt).map_err(|_| {
                    "Installation publication has not settled. The local API remains available."
                        .to_string()
                })?;
                music.settle().map_err(|_| {
                    "Music files have not settled. The local API remains available.".to_string()
                })?;
                runtime.settle().map_err(|_| {
                    "Java runtime files have not settled. The local API remains available."
                        .to_string()
                })?;
                library.try_preserve().map_err(|_| {
                    "Managed files have not settled. The local API remains available.".to_string()
                })
            }));
        }
        let result = pending
            .as_mut()
            .expect("file settlement owner exists")
            .await
            .unwrap_or_else(|_| {
                Err(
                    "Filesystem settlement was interrupted. The local API remains available."
                        .into(),
                )
            });
        pending.take();
        result
    }
}

pub async fn start_desktop(extra_origin: Option<&str>) -> Result<DesktopServices, StartupError> {
    start_profile(
        configured_profile()?,
        extra_origin,
        true,
        configured_telemetry(),
    )
    .await
}

pub async fn start_browser(extra_origin: Option<&str>) -> Result<DesktopServices, StartupError> {
    start_profile(
        configured_profile()?,
        extra_origin,
        false,
        configured_telemetry(),
    )
    .await
}

fn configured_profile() -> Result<PathBuf, String> {
    Ok(match std::env::var_os("AXIAL_REWRITE_PROFILE") {
        Some(path) => PathBuf::from(path),
        None => dirs::data_local_dir()
            .ok_or("The application data directory is unavailable.")?
            .join(DEVELOPMENT_APPLICATION_ID),
    })
}

fn configured_telemetry() -> Option<CollectorConfig> {
    let key = std::env::var("AXIAL_REWRITE_TELEMETRY_API_KEY").ok()?;
    let environment = std::env::var("AXIAL_REWRITE_TELEMETRY_ENVIRONMENT")
        .ok()
        .and_then(|label| TelemetryEnvironment::from_label(&label))
        .unwrap_or(if cfg!(debug_assertions) {
            TelemetryEnvironment::Development
        } else {
            TelemetryEnvironment::Production
        });
    match std::env::var("AXIAL_REWRITE_TELEMETRY_HOST") {
        Ok(host) => CollectorConfig::new(&key, &host, environment).ok(),
        Err(std::env::VarError::NotPresent) => CollectorConfig::posthog(&key, environment).ok(),
        Err(std::env::VarError::NotUnicode(_)) => None,
    }
}

/// Explicit profile injection keeps browser, desktop, and acceptance tests on
/// the same composition without changing process-global environment variables.
pub async fn start_in_profile(
    profile_root: PathBuf,
    extra_origin: Option<&str>,
) -> Result<DesktopServices, StartupError> {
    start_profile(profile_root, extra_origin, false, None).await
}

async fn start_profile(
    profile_root: PathBuf,
    extra_origin: Option<&str>,
    native_login: bool,
    collector: Option<CollectorConfig>,
) -> Result<DesktopServices, StartupError> {
    let telemetry = Arc::new(Telemetry::new(collector));
    let result = start_profile_inner(
        profile_root,
        extra_origin,
        native_login,
        telemetry.clone(),
        #[cfg(test)]
        None,
        #[cfg(test)]
        None,
    )
    .await;
    if result.is_err() && telemetry.report_startup_failure() {
        telemetry.flush_once().await;
    }
    result
}

#[cfg(test)]
async fn start_profile_with_test_endpoints(
    profile_root: PathBuf,
    endpoints: axial_minecraft::download::InstallTestEndpoints,
) -> Result<DesktopServices, StartupError> {
    start_profile_inner(
        profile_root,
        None,
        false,
        Arc::new(Telemetry::new(None)),
        Some(endpoints),
        None,
    )
    .await
}

#[cfg(test)]
async fn start_profile_with_performance_test_inputs(
    profile_root: PathBuf,
    content_base_url: String,
    transfers: axial_performance::ManagedArtifactTransferResolver,
) -> Result<DesktopServices, StartupError> {
    start_profile_inner(
        profile_root,
        None,
        false,
        Arc::new(Telemetry::new(None)),
        None,
        Some((content_base_url, transfers)),
    )
    .await
}

async fn start_profile_inner(
    profile_root: PathBuf,
    extra_origin: Option<&str>,
    native_login: bool,
    telemetry: Arc<Telemetry>,
    #[cfg(test)] test_endpoints: Option<axial_minecraft::download::InstallTestEndpoints>,
    #[cfg(test)] performance_inputs: Option<(
        String,
        axial_performance::ManagedArtifactTransferResolver,
    )>,
) -> Result<DesktopServices, StartupError> {
    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .map_err(|_| "Could not bind the local API.".to_string())?;
    let authority = LocalApiAuthority::new(
        listener
            .local_addr()
            .map_err(|_| "Local API address is unavailable.")?,
        extra_origin,
    )?;
    let telemetry_for_init = telemetry.clone();
    let (profile, library, metadata, settings, accounts, consent, identity, inspector_override) = tokio::task::spawn_blocking(move || {
        let profile = admit_profile(&profile_root)?;
        let library_id = LibraryId::parse(&profile.identity.profile_id.to_string()).map_err(|_| "The replacement profile identity is invalid.")?;
        let library = match LibraryLifecycle::open_with_id(&profile.root, library_id) {
            LibraryOpenOutcome::Ready(library) => library,
            LibraryOpenOutcome::NoEffect(_) => return Err("The replacement profile is already open or unavailable. Existing files have been preserved.".into()),
            outcome @ LibraryOpenOutcome::Unresolved(_) => return Err(StartupError {
                message: "Replacement profile admission is unresolved. Startup is blocked while its filesystem obligations remain owned.".into(),
                root: Some(outcome),
                runtime: None,
                interrupted_reset: false,
            }),
        };
        match library.interrupted_root_reset() {
            Ok(false) => {}
            Ok(true) => {
                let mut failure = StartupError::with_library(
                    "A previous reset was interrupted. The profile is preserved until you confirm a new reset.".into(),
                    library,
                );
                failure.interrupted_reset = true;
                return Err(failure);
            }
            Err(_) => return Err(StartupError::with_library(
                "The profile reset record could not be verified. Existing files have been preserved.".into(),
                library,
            )),
        }
        let retained_library = library.clone();
        (|| {
        library.restore_startup_selection(library_id)
            .map_err(|error| error.to_string())?;
        let metadata = Arc::new(MetadataStore::open(profile.root.join("metadata.sqlite"))
            .map_err(|_| "Could not open replacement metadata. Existing data has been preserved.".to_string())?);
        metadata.migrate(&[SETTINGS_MIGRATION, ACCOUNTS_MIGRATION,
            axial_app::instances::directory::MIGRATION, axial_app::instances::delete::MIGRATION,
            axial_app::instances::create::MIGRATION,
            axial_app::install::queue::MIGRATION,
            axial_app::content::install::MIGRATION, axial_app::performance::rules::MIGRATION,
            axial_app::performance::mutation::MIGRATION,
            axial_app::performance::benchmarks::MIGRATION,
            axial_app::skins::store::MIGRATION,
            axial_app::launch::coordinator::INTENT_MIGRATION])
            .map_err(|_| "Could not migrate replacement metadata. Existing data has been preserved.".to_string())?;
        let settings = Arc::new(SettingsStore::new_with_telemetry_identity(metadata.clone(), telemetry_for_init.export_configured())
            .map_err(|error| error.to_string())?);
        let accounts = Arc::new(AccountDirectory::new(metadata.clone()).map_err(|error| error.to_string())?);
        let consent = settings.current().map_err(|error| error.to_string())?.telemetry_enabled;
        let identity = settings.telemetry_identity().map_err(|error| error.to_string())?;
        let inspector_override = settings.list_flags().map_err(|error| error.to_string())?.flags.iter()
            .any(|flag| flag.key == axial_app::settings::STATE_INSPECTOR_FLAG && flag.enabled != flag.default_enabled);
        Ok::<_, String>((profile, library, metadata, settings, accounts, consent, identity, inspector_override))
        })().map_err(|message| StartupError::with_library(message, retained_library))
    }).await.map_err(|_| "Replacement profile initialization was interrupted.".to_string())??;
    // Recovery can commit real feature outcomes. It must observe the persisted
    // consent and identity loaded above, never the default startup state.
    telemetry
        .consent_change()
        .await
        .publish(consent, identity.as_deref());
    let tasks = TaskOwner::new(64).map_err(|_| {
        StartupError::with_library(
            "Could not create the application task owner.".to_string(),
            library.clone(),
        )
    })?;
    let credentials = Arc::new(CredentialStore::with_task_owner(
        profile.identity.profile_id,
        tasks.clone(),
    ));
    let auth = Arc::new(AuthService::new(
        accounts.clone(),
        credentials,
        tasks.clone(),
    ));
    let (
        instances,
        installs,
        runtimes,
        catalog,
        performance,
        skins,
        reports,
        sessions,
        launch,
        setup,
        benchmarks,
        content_mutations,
        content_routes,
        resources,
        music,
    ) = {
        let library_for_error = library.clone();
        let library = library.clone();
        let tasks = tasks.clone();
        let settings = settings.clone();
        let accounts = accounts.clone();
        let auth = auth.clone();
        let telemetry = telemetry.clone();
        tokio::task::spawn_blocking(move || {
            let retained_library = library.clone();
            let mut retained_runtime = None;
            (|| {
                let exclusions = Exclusions::default();
                let directories = InstanceDirectories::new(
                    Registry::new(metadata.clone()),
                    library.clone(),
                    exclusions.clone(),
                );
                let reports =
                    LaunchReportStore::new(metadata.clone()).map_err(|error| error.to_string())?;
                let intents =
                    LaunchIntents::restore(metadata.clone(), reports.clone(), &directories)
                        .map_err(|error| error.to_string())?;
                let runtime_cache = library.runtime_cache().map_err(|error| error.to_string())?;
                retained_runtime = Some(runtime_cache.clone());
                let instances = Arc::new(
                    InstanceService::new(directories.clone(), tasks.clone())
                        .with_telemetry(telemetry.clone()),
                );
                let runtimes = RuntimeDiscovery::new(runtime_cache.clone(), tasks.clone());
                #[cfg(test)]
                let runtimes = match test_endpoints.as_ref() {
                    Some(endpoints) => runtimes.with_test_endpoints(endpoints.clone()),
                    None => runtimes,
                };
                let installs = InstallQueue::new(
                    metadata.clone(),
                    library.clone(),
                    exclusions,
                    tasks.clone(),
                    runtime_cache,
                )
                .map_err(|error| error.to_string())?
                .with_telemetry(telemetry.clone());
                #[cfg(test)]
                let installs = match test_endpoints {
                    Some(endpoints) => installs.with_test_endpoints(endpoints),
                    None => installs,
                };
                let client = ProviderClient::new(ClientConfig::default())
                    .map_err(|error| error.to_string())?;
                let catalog = Arc::new(Catalog::new(client.clone()));
                let content =
                    ContentService::new(client.clone()).map_err(|error| error.to_string())?;
                let transfers = axial_app::performance::public_transfer_resolver();
                #[cfg(test)]
                let (content, transfers) = match performance_inputs {
                    Some((base_url, transfers)) => {
                        let origin = url::Url::parse(&base_url)
                            .map_err(|error| error.to_string())?
                            .origin()
                            .ascii_serialization();
                        let origins =
                            axial_app::network::OriginPolicy::loopback_for_tests([origin], 0)
                                .map_err(|error| error.to_string())?;
                        let content =
                            ContentService::with_base_url(client.clone(), base_url, origins)
                                .map_err(|error| error.to_string())?;
                        (content, transfers)
                    }
                    None => (content, transfers),
                };
                let content = Arc::new(content);
                let performance = PerformanceService::new(
                    metadata.clone(),
                    directories.clone(),
                    tasks.clone(),
                    content.clone(),
                    transfers,
                )
                .map_err(|error| error.to_string())?;
                let content_mutations = Arc::new(
                    ContentMutations::new(directories.clone(), client, tasks.clone())
                        .with_performance(performance.clone()),
                );
                let installs =
                    Arc::new(installs.with_content(content.clone(), content_mutations.clone()));
                let resources = Arc::new(ResourceService::new(
                    directories.clone(),
                    content_mutations.as_ref().clone(),
                    tasks.clone(),
                ));
                let music = Arc::new(
                    MusicService::new(library.clone(), tasks.clone())
                        .map_err(|error| error.to_string())?,
                );
                let content_routes = Arc::new(routes::content::ContentRoutes::new(
                    content.as_ref().clone(),
                    content_mutations.as_ref().clone(),
                    installs.clone(),
                ));
                let root_pin = library
                    .admit_application_root()
                    .map_err(|error| error.to_string())?;
                let skin_library = Arc::new(SavedSkinLibrary::new(
                    SavedSkinStore::new(metadata.clone()),
                    root_pin.clone(),
                ));
                let skins = ProfileMedia::new(
                    skin_library,
                    accounts.clone(),
                    auth.clone(),
                    tasks.clone(),
                    root_pin,
                )
                .map_err(|error| error.to_string())?;
                let sessions = SessionManager::with_reports(tasks.clone(), reports.clone());
                let launch = Arc::new(
                    LaunchCoordinator::new(
                        directories,
                        accounts.clone(),
                        settings.clone(),
                        installs.as_ref().clone(),
                        runtimes.clone(),
                        performance.clone(),
                        auth.clone(),
                        skins.clone(),
                        sessions.clone(),
                        tasks.clone(),
                    )
                    .with_intents(intents)
                    .with_telemetry(telemetry.clone()),
                );
                let benchmarks = Arc::new(
                    BenchmarkService::new(
                        metadata,
                        instances.registry().clone(),
                        Arc::new(reports.clone()),
                        launch.as_ref().clone(),
                        sessions.clone(),
                        tasks.clone(),
                    )
                    .map_err(|error| error.to_string())?,
                );
                let setup = Arc::new(
                    SetupService::new(
                        instances.clone(),
                        catalog.clone(),
                        installs.clone(),
                        settings.clone(),
                        launch.clone(),
                    )
                    .with_content(content, content_mutations.clone()),
                );
                Ok::<_, String>((
                    instances,
                    installs,
                    runtimes,
                    catalog,
                    performance,
                    skins,
                    reports,
                    sessions,
                    launch,
                    setup,
                    benchmarks,
                    content_mutations,
                    content_routes,
                    resources,
                    music,
                ))
            })()
            .map_err(|message| StartupError {
                message,
                root: Some(LibraryOpenOutcome::Ready(retained_library)),
                runtime: retained_runtime,
                interrupted_reset: false,
            })
        })
        .await
        .map_err(|_| {
            StartupError::with_library(
                "Feature initialization was interrupted.".to_string(),
                library_for_error,
            )
        })??
    };
    let config = routes::config::ConfigRouteState::new(
        settings.clone(),
        accounts.clone(),
        telemetry.clone(),
        tasks.clone(),
    )
    .map_err(|error| StartupError::with_library(error.to_string(), library.clone()))?;
    if instances.recover_pending().await.is_err() {
        tracing::warn!(
            "instance startup settlement remains unresolved; affected instances stay unavailable"
        );
    }
    if installs.recover_interrupted().await.is_err() {
        tracing::warn!(
            "installation startup settlement remains unresolved; affected versions stay unavailable"
        );
    }
    let recovering = performance.clone();
    match tasks.try_spawn(performance.clone(), move |_| async move {
        recovering.recover_pending().await
    }) {
        Ok(recovery) => {
            if !matches!(recovery.join().await, Ok(Ok(_))) {
                tracing::warn!(
                    "Performance startup settlement remains unresolved; affected instances stay unavailable"
                );
            }
        }
        Err(_) => tracing::warn!(
            "Performance startup settlement could not begin; affected instances stay unavailable"
        ),
    }
    let updates = UpdateService::new(
        env!("CARGO_PKG_VERSION"),
        std::env::consts::OS,
        std::env::consts::ARCH,
    );
    let folders = Arc::new(FolderService::new(
        instances.clone(),
        tasks.clone(),
        Arc::new(open_folder::PlatformFolderOpener),
    ));
    let router = Router::new()
        .merge(routes::status_router(settings.clone(), library.clone()))
        .merge(routes::config::router(config))
        .merge(routes::flags::router(
            settings.clone(),
            telemetry.clone(),
            tasks.clone(),
        ))
        .merge(routes::accounts::router(auth.clone()))
        .merge(routes::auth::router(auth.clone(), native_login))
        .merge(routes::telemetry::router(telemetry.clone()))
        .merge(routes::update::router(updates.clone()))
        .merge(routes::instances::router(
            instances.clone(),
            setup.clone(),
            sessions.clone(),
        ))
        .merge(routes::setup::router(setup.clone()))
        .merge(routes::install::router(installs.clone(), setup.clone()))
        .merge(routes::loaders::router(
            library.clone(),
            catalog.clone(),
            tasks.clone(),
        ))
        .merge(routes::versions::router(
            library.clone(),
            catalog.clone(),
            tasks.clone(),
        ))
        .merge(routes::java::router(runtimes))
        .merge(routes::launch::router(
            launch.as_ref().clone(),
            sessions.clone(),
        ))
        .merge(routes::launch::reports_router(reports))
        .merge(routes::skin::router(skins.clone()))
        .merge(routes::performance::router(
            Arc::new(performance.clone()),
            settings.clone(),
        ))
        .merge(routes::benchmarks::router(
            benchmarks.clone(),
            Arc::new(performance.clone()),
        ))
        .merge(routes::content::router(content_routes))
        .merge(routes::resources::router(resources.clone()))
        .merge(routes::resources::folders_router(folders))
        .merge(routes::system::router())
        .merge(routes::music::router(music.clone()));
    let router = transport::protected_router(router, authority.clone());
    #[cfg(feature = "embedded-frontend")]
    let router = if native_login {
        router
    } else {
        frontend::router(router)
    };
    axial_app::telemetry::install_panic_capture(&telemetry);
    telemetry.emit(TelemetryEvent::AppStarted {
        state_inspector: inspector_override,
    });
    let (shutdown, receiver) = watch::channel(false);
    let telemetry_worker = {
        let telemetry = telemetry.clone();
        let receiver = receiver.clone();
        tokio::spawn(async move {
            telemetry.run(receiver).await;
            Ok(())
        })
    };
    let rules_worker = {
        let rules = performance.rules().clone();
        let tasks = tasks.clone();
        let receiver = receiver.clone();
        tokio::spawn(async move {
            rules.run(tasks, receiver).await;
            Ok(())
        })
    };
    let serving_telemetry = telemetry.clone();
    let join = tokio::spawn(async move {
        let result = serve(listener, router, receiver).await;
        if result.is_err() && serving_telemetry.report_startup_failure() {
            serving_telemetry.flush_once().await;
        }
        result
    });
    let server = Arc::new(ServerHandle {
        authority,
        tasks: tasks.clone(),
        library: library.clone(),
        instances: instances.clone(),
        setup,
        installs: installs.clone(),
        sessions: sessions.clone(),
        skins: skins.clone(),
        performance: performance.clone(),
        shutdown,
        runtime_cache: installs.runtime_cache().clone(),
        content_mutations: content_mutations.clone(),
        resources: resources.clone(),
        music: music.clone(),
        serving: Mutex::new(ServerTask {
            join: Some(join),
            result: None,
        }),
        telemetry_worker: Mutex::new(ServerTask {
            join: Some(telemetry_worker),
            result: None,
        }),
        rules_worker: Mutex::new(ServerTask {
            join: Some(rules_worker),
            result: None,
        }),
        shutdown_settled: AtomicBool::new(false),
        file_settlement: Mutex::new(None),
    });
    installs.resume_queued();
    if benchmarks.resume_interrupted_drivers().is_err() {
        tracing::warn!(
            "benchmark restart remains unresolved; driver status retains the interrupted run"
        );
    }
    Ok(DesktopServices {
        server,
        tasks,
        profile_root: profile.root,
        library,
        settings,
        accounts,
        auth,
        telemetry,
        updates,
        catalog,
        instances,
        installs,
        sessions,
        skins,
        performance,
        launch,
        benchmarks,
        content_mutations,
        resources,
        music,
    })
}

async fn serve(
    listener: TcpListener,
    router: Router,
    mut shutdown: watch::Receiver<bool>,
) -> io::Result<()> {
    use hyper_util::{
        rt::{TokioExecutor, TokioIo},
        server::conn::auto::Builder,
        service::TowerToHyperService,
    };
    let mut connections = JoinSet::new();
    loop {
        if *shutdown.borrow() {
            break;
        }
        tokio::select! {
            biased;
            _ = shutdown.changed() => break,
            Some(_) = connections.join_next(), if !connections.is_empty() => {},
            accepted = listener.accept() => {
                let (stream, peer) = accepted?;
                if !peer.ip().is_loopback() { continue; }
                if connections.len() >= 128 { continue; }
                let service = TowerToHyperService::new(router.clone());
                let mut stopped = shutdown.clone();
                connections.spawn(async move {
                    let builder = Builder::new(TokioExecutor::new());
                    let connection = builder.serve_connection_with_upgrades(TokioIo::new(stream), service);
                    tokio::pin!(connection);
                    if !*stopped.borrow() {
                        tokio::select! {
                            _ = &mut connection => return,
                            _ = stopped.changed() => {},
                        }
                    }
                    connection.as_mut().graceful_shutdown();
                    // Domain effects are already settled. Only transport I/O
                    // may be dropped at this deadline, including idle SSE.
                    let _ = tokio::time::timeout(HTTP_SHUTDOWN_TIMEOUT, &mut connection).await;
                });
            }
        }
    }
    drop(listener);
    while connections.join_next().await.is_some() {}
    Ok(())
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ProfileIdentity {
    application_id: String,
    profile_id: uuid::Uuid,
}

struct AdmittedProfile {
    root: PathBuf,
    identity: ProfileIdentity,
}

fn admit_profile(requested: &Path) -> Result<AdmittedProfile, String> {
    const INVALID: &str = "Use an absolute, isolated replacement profile directory.";
    const UNKNOWN: &str = "The selected directory is not an admitted replacement profile. Existing files have been preserved.";
    if !requested.is_absolute()
        || requested
            .components()
            .any(|part| matches!(part, Component::ParentDir | Component::CurDir))
    {
        return Err(INVALID.into());
    }
    // Resolve the existing ancestor before creating anything. An alias into an
    // installed profile must be refused just like its direct spelling.
    let mut ancestor = requested;
    let mut missing = Vec::new();
    while !ancestor.try_exists().map_err(|_| UNKNOWN)? {
        missing.push(ancestor.file_name().ok_or(INVALID)?.to_owned());
        ancestor = ancestor.parent().ok_or(INVALID)?;
    }
    let mut root = fs::canonicalize(ancestor).map_err(|_| UNKNOWN)?;
    for component in missing.iter().rev() {
        root.push(component);
    }
    if root.parent().is_none()
        || root == dirs::home_dir().unwrap_or_default()
        || root.components().any(|part| {
            matches!(part, Component::Normal(name)
            if name == "dev.mateoltd.axial" || name == "com.mateoltd.axial")
        })
    {
        return Err(INVALID.into());
    }
    fs::create_dir_all(&root).map_err(|_| UNKNOWN)?;
    if !fs::symlink_metadata(&root).map_err(|_| UNKNOWN)?.is_dir() {
        return Err(UNKNOWN.into());
    }
    let marker = root.join(PROFILE_MARKER);
    let identity = match fs::symlink_metadata(&marker) {
        Ok(metadata) => {
            if !metadata.is_file() || metadata.len() > 4096 {
                return Err(UNKNOWN.into());
            }
            let mut bytes = Vec::new();
            fs::File::open(&marker)
                .map_err(|_| UNKNOWN)?
                .take(4097)
                .read_to_end(&mut bytes)
                .map_err(|_| UNKNOWN)?;
            if bytes.len() > 4096 {
                return Err(UNKNOWN.into());
            }
            let identity: ProfileIdentity = serde_json::from_slice(&bytes).map_err(|_| UNKNOWN)?;
            if identity.application_id != DEVELOPMENT_APPLICATION_ID || identity.profile_id.is_nil()
            {
                return Err(UNKNOWN.into());
            }
            identity
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            if fs::read_dir(&root).map_err(|_| UNKNOWN)?.next().is_some() {
                return Err(UNKNOWN.into());
            }
            let identity = ProfileIdentity {
                application_id: DEVELOPMENT_APPLICATION_ID.into(),
                profile_id: uuid::Uuid::new_v4(),
            };
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = options.open(&marker).map_err(|_| UNKNOWN)?;
            let bytes = serde_json::to_vec(&identity).map_err(|_| UNKNOWN)?;
            file.write_all(&bytes)
                .and_then(|_| file.sync_all())
                .map_err(|_| UNKNOWN)?;
            identity
        }
        Err(_) => return Err(UNKNOWN.into()),
    };
    Ok(AdmittedProfile { root, identity })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn configured_rules_refreshes_on_startup_without_an_http_command() {
        use ed25519_dalek::{Signer, SigningKey};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let temporary =
            tempfile::tempdir_in(fs::canonicalize(std::env::temp_dir()).unwrap()).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/rules", listener.local_addr().unwrap());
        let mut manifest = axial_performance::builtin_manifest().unwrap();
        manifest.generated_at = "2001-01-01T00:00:00Z".into();
        let body = axial_performance::canonical_manifest_payload(&manifest).unwrap();
        let key = SigningKey::from_bytes(&[23; 32]);
        let signature = hex::encode(key.sign(&body).to_bytes());
        let provider = tokio::spawn(async move {
            let (mut stream, _) = tokio::time::timeout(Duration::from_secs(15), listener.accept())
                .await
                .expect("startup never contacted configured rules provider")
                .unwrap();
            let mut request = Vec::new();
            let mut chunk = [0; 1024];
            while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                let count = tokio::time::timeout(Duration::from_secs(2), stream.read(&mut chunk))
                    .await
                    .unwrap()
                    .unwrap();
                assert!(count > 0 && request.len() + count <= 8192);
                request.extend_from_slice(&chunk[..count]);
            }
            assert!(request.starts_with(b"GET /rules HTTP/1.1\r\n"));
            stream.write_all(format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nx-axial-rules-signature-ed25519: {signature}\r\nConnection: close\r\n\r\n",
                body.len(),
            ).as_bytes()).await.unwrap();
            stream.write_all(&body).await.unwrap();
        });
        let output = tokio::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "tests::configured_rules_startup_helper",
                "--ignored",
                "--nocapture",
            ])
            .env(
                "AXIAL_TEST_RULES_PROFILE",
                temporary.path().join("replacement"),
            )
            .env(axial_performance::PERFORMANCE_RULES_URL_ENV, url)
            .env(
                axial_performance::PERFORMANCE_RULES_PUBLIC_KEY_ENV,
                hex::encode(key.verifying_key().to_bytes()),
            )
            .env("AXIAL_PERFORMANCE_RULES_REFRESH_INTERVAL_SECONDS", "900")
            .output()
            .await
            .unwrap();
        let served = provider.await;
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        served.unwrap();
    }

    #[tokio::test]
    #[ignore = "configured startup child isolates process-wide provider configuration"]
    async fn configured_rules_startup_helper() {
        let root = PathBuf::from(std::env::var_os("AXIAL_TEST_RULES_PROFILE").unwrap());
        let services = start_in_profile(root, None).await.unwrap();
        let refreshed = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let status = services.performance.rules().status().status;
                if status.rule_source == axial_performance::RuleSource::Remote
                    && status.generated_at == "2001-01-01T00:00:00Z"
                    && services.tasks.status().is_idle()
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .is_ok();
        let idle_close = services.tasks.try_close_idle().is_ok();
        let shutdown = services.server.shutdown().await;
        let settled = services.server.is_shutdown_settled();
        drop(services);
        assert!(
            refreshed,
            "configured rules did not publish through actual startup"
        );
        assert!(
            idle_close,
            "sleeping rules worker blocked native idle closure"
        );
        shutdown.unwrap();
        assert!(settled);
    }

    #[test]
    fn profile_identity_persists_and_unknown_files_are_preserved() {
        let temporary = tempfile::tempdir().unwrap();
        let isolated = temporary.path().join("replacement");
        let first = admit_profile(&isolated).unwrap();
        let second = admit_profile(&isolated).unwrap();
        assert_eq!(first.identity.profile_id, second.identity.profile_id);
        let unknown = temporary.path().join("unknown");
        fs::create_dir(&unknown).unwrap();
        fs::write(unknown.join("keep.txt"), "user data").unwrap();
        assert!(admit_profile(&unknown).is_err());
        assert_eq!(
            fs::read_to_string(unknown.join("keep.txt")).unwrap(),
            "user data"
        );
        assert!(!unknown.join(PROFILE_MARKER).exists());
        assert!(admit_profile(Path::new("relative-profile")).is_err());
        assert!(
            admit_profile(&temporary.path().join("dev.mateoltd.axial").join("rewrite")).is_err()
        );
        assert!(!temporary.path().join("dev.mateoltd.axial").exists());
    }

    #[tokio::test]
    async fn loopback_server_authenticates_real_settings_and_reopens_profile() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("replacement");
        let services = start_in_profile(root.clone(), Some("http://localhost:1420"))
            .await
            .unwrap();
        let bootstrap = services.server.bootstrap();
        let client = reqwest::Client::new();
        let url = format!("{}/api/v1/config", bootstrap.base_url);
        assert_eq!(
            client.get(&url).send().await.unwrap().status(),
            reqwest::StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            client
                .get(&url)
                .header(transport::CAPABILITY_HEADER, &bootstrap.capability)
                .header("origin", "http://localhost:1421")
                .send()
                .await
                .unwrap()
                .status(),
            reqwest::StatusCode::FORBIDDEN
        );
        let settings: serde_json::Value = client
            .get(&url)
            .header(transport::CAPABILITY_HEADER, &bootstrap.capability)
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(settings["revision"], 0);
        let updated: serde_json::Value = client
            .put(&url)
            .header(transport::CAPABILITY_HEADER, &bootstrap.capability)
            .json(&serde_json::json!({"expected_revision": 0, "theme": "nether"}))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(updated["revision"], 1);
        assert_eq!(updated["theme"], "nether");
        let account: serde_json::Value = client
            .post(format!("{}/api/v1/accounts/offline", bootstrap.base_url))
            .header(transport::CAPABILITY_HEADER, &bootstrap.capability)
            .json(&serde_json::json!({"username":"FixturePlayer", "expected_selection_revision":0}))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(account["status"], "account_created");
        assert_eq!(account["selection_revision"], 1);
        services.server.shutdown().await.unwrap();
        assert!(client.get(&url).send().await.is_err());
        drop(services);
        let reopened = start_in_profile(root, None).await.unwrap();
        assert_ne!(bootstrap.capability, reopened.server.bootstrap().capability);
        assert_eq!(reopened.settings.current().unwrap().revision, 1);
        assert_eq!(
            reopened.settings.current().unwrap().theme.as_str(),
            "nether"
        );
        assert_eq!(
            reopened.accounts.capture_selected().unwrap().display_name(),
            "FixturePlayer"
        );
        reopened.server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn dropping_waiter_does_not_lose_the_http_join() {
        let temporary = tempfile::tempdir().unwrap();
        let services = start_in_profile(temporary.path().join("replacement"), None)
            .await
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(5), services.server.wait())
                .await
                .is_err()
        );
        services.server.shutdown().await.unwrap();
        services.server.wait().await.unwrap();
    }

    #[tokio::test]
    async fn cancelled_shutdown_keeps_http_available_until_domain_work_settles() {
        let temporary = tempfile::tempdir().unwrap();
        let services = start_in_profile(temporary.path().join("replacement"), None)
            .await
            .unwrap();
        let (release, released) = tokio::sync::oneshot::channel();
        let work = services
            .tasks
            .try_spawn((), move |_| async move {
                released.await.unwrap();
            })
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(5), services.server.shutdown())
                .await
                .is_err()
        );
        assert!(services.tasks.status().closing);
        let bootstrap = services.server.bootstrap();
        let response = reqwest::Client::new()
            .get(format!("{}/api/v1/config", bootstrap.base_url))
            .header(transport::CAPABILITY_HEADER, bootstrap.capability)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        release.send(()).unwrap();
        work.join().await.unwrap();
        services.server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn lingering_stream_cannot_block_http_shutdown_forever() {
        use axum::{
            response::{Sse, sse::Event},
            routing::get,
        };
        use std::convert::Infallible;
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let router = Router::new().route(
            "/stream",
            get(|| async {
                Sse::new(futures_util::stream::pending::<Result<Event, Infallible>>())
            }),
        );
        let (shutdown, receiver) = watch::channel(false);
        let serving = tokio::spawn(serve(listener, router, receiver));
        let _stream = reqwest::Client::new()
            .get(format!("http://{address}/stream"))
            .send()
            .await
            .unwrap();
        shutdown.send_replace(true);
        tokio::time::timeout(HTTP_SHUTDOWN_TIMEOUT + Duration::from_secs(2), serving)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }

    async fn telemetry_collector() -> (
        String,
        tokio::sync::mpsc::UnboundedReceiver<serde_json::Value>,
        watch::Sender<bool>,
        JoinHandle<io::Result<()>>,
    ) {
        use axum::{Json, extract::State, http::StatusCode, routing::post};
        async fn collect(
            State(batches): State<tokio::sync::mpsc::UnboundedSender<serde_json::Value>>,
            Json(batch): Json<serde_json::Value>,
        ) -> StatusCode {
            batches.send(batch).unwrap();
            StatusCode::NO_CONTENT
        }
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let host = format!("http://{}", listener.local_addr().unwrap());
        let (batches, received) = tokio::sync::mpsc::unbounded_channel();
        let (stop, stopped) = watch::channel(false);
        let collecting = tokio::spawn(serve(
            listener,
            Router::new()
                .route("/batch/", post(collect))
                .with_state(batches),
            stopped,
        ));
        (host, received, stop, collecting)
    }

    #[tokio::test]
    async fn configured_telemetry_restores_consent_and_joins_final_flush() {
        let (host, mut received, stop, collecting) = telemetry_collector().await;
        let collector = || {
            Some(
                CollectorConfig::new("phc_fixture_key", &host, TelemetryEnvironment::Test).unwrap(),
            )
        };
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("replacement");
        let services = start_profile(root.clone(), None, false, collector())
            .await
            .unwrap();
        assert!(!services.telemetry.report_startup_failure());
        let bootstrap = services.server.bootstrap();
        reqwest::Client::new()
            .put(format!("{}/api/v1/config", bootstrap.base_url))
            .header(transport::CAPABILITY_HEADER, bootstrap.capability)
            .json(&serde_json::json!({"expected_revision":0,"telemetry_enabled":true}))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap();
        let identity = services.settings.telemetry_identity().unwrap().unwrap();
        assert!(services.telemetry.report_startup_failure());
        assert!(!services.telemetry.report_startup_failure());
        services.server.shutdown().await.unwrap();
        drop(services);
        let first_batches: Vec<_> = std::iter::from_fn(|| received.try_recv().ok()).collect();
        // The initial launch occurred without consent. The reopened launch
        // must observe persisted consent before publishing its startup event.
        let reopened = start_profile(root, None, false, collector()).await.unwrap();
        assert_eq!(
            reopened.settings.telemetry_identity().unwrap().as_deref(),
            Some(identity.as_str())
        );
        assert!(reopened.telemetry.report_startup_failure());
        assert!(!reopened.telemetry.report_startup_failure());
        reopened.server.shutdown().await.unwrap();
        let reopened_batches: Vec<_> = std::iter::from_fn(|| received.try_recv().ok()).collect();
        stop.send_replace(true);
        collecting.await.unwrap().unwrap();
        for batches in [&first_batches, &reopened_batches] {
            let failures: Vec<_> = batches
                .iter()
                .flat_map(|batch| batch["batch"].as_array().unwrap())
                .filter(|event| event["properties"]["$exception_fingerprint"] == "startup_failed")
                .collect();
            assert_eq!(failures.len(), 1);
            assert_eq!(failures[0]["properties"]["distinct_id"], identity);
        }
        assert!(
            reopened_batches
                .iter()
                .flat_map(|batch| batch["batch"].as_array().unwrap())
                .any(|event| event["event"] == "app_started")
        );
    }

    #[tokio::test]
    async fn startup_failure_flushes_only_with_readable_saved_consent() {
        let (host, mut received, stop, collecting) = telemetry_collector().await;
        let collector = || {
            Some(
                CollectorConfig::new("phc_fixture_key", &host, TelemetryEnvironment::Test).unwrap(),
            )
        };
        let mut observations = Vec::new();
        for (enabled, configured, corrupt) in [
            (false, true, false),
            (true, true, false),
            (true, false, false),
            (true, true, true),
        ] {
            let temporary =
                tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
            let root = temporary.path().join("profile");
            let services = start_profile(root.clone(), None, false, collector())
                .await
                .unwrap();
            let bootstrap = services.server.bootstrap();
            reqwest::Client::new()
                .put(format!("{}/api/v1/config", bootstrap.base_url))
                .header(transport::CAPABILITY_HEADER, bootstrap.capability)
                .json(&serde_json::json!({"expected_revision":0,"telemetry_enabled":enabled}))
                .send()
                .await
                .unwrap()
                .error_for_status()
                .unwrap();
            let identity = services.settings.telemetry_identity().unwrap();
            services.server.shutdown().await.unwrap();
            drop(services);
            while received.try_recv().is_ok() {}
            fs::rename(root.join("runtime"), root.join("preserved-runtime")).unwrap();
            let blocker = b"synthetic private startup blocker";
            fs::write(root.join("runtime"), blocker).unwrap();
            if corrupt {
                fs::rename(
                    root.join("metadata.sqlite"),
                    root.join("preserved-metadata.sqlite"),
                )
                .unwrap();
                fs::write(root.join("metadata.sqlite"), blocker).unwrap();
            }
            let marker = fs::read(root.join(PROFILE_MARKER)).unwrap();
            let failure = start_profile(
                root.clone(),
                None,
                false,
                configured.then(collector).flatten(),
            )
            .await
            .err()
            .expect("the real filesystem blocker must refuse startup");
            failure.try_preserve().unwrap();
            assert_eq!(fs::read(root.join("runtime")).unwrap(), blocker);
            assert_eq!(fs::read(root.join(PROFILE_MARKER)).unwrap(), marker);
            observations.push((
                enabled && configured && !corrupt,
                identity,
                received.try_recv().ok(),
            ));
            assert!(received.try_recv().is_err());
        }
        stop.send_replace(true);
        collecting.await.unwrap().unwrap();
        for (expected, identity, batch) in observations {
            assert_eq!(
                batch.is_some(),
                expected,
                "post-consent startup failure must flush before returning"
            );
            if let Some(batch) = batch {
                let events = batch["batch"].as_array().unwrap();
                assert_eq!(events.len(), 1);
                assert_eq!(events[0]["event"], "$exception");
                let properties = &events[0]["properties"];
                assert_eq!(properties["distinct_id"], identity.unwrap());
                assert_eq!(properties["$exception_fingerprint"], "startup_failed");
                assert_eq!(properties["area"], "startup");
                assert_eq!(
                    properties["$exception_list"],
                    serde_json::json!([
                        {"type":"startup_failed", "value":"Application startup failed."}
                    ])
                );
                assert!(!batch.to_string().contains("private startup blocker"));
            }
        }
    }

    #[tokio::test]
    async fn saved_skin_media_ticket_reads_real_png_and_cannot_read_config() {
        let temporary = tempfile::tempdir().unwrap();
        let services = start_in_profile(temporary.path().join("replacement"), None)
            .await
            .unwrap();
        let bootstrap = services.server.bootstrap();
        let client = reqwest::Client::new();
        let mut png = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut png, 64, 64);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            encoder
                .write_header()
                .unwrap()
                .write_image_data(&[24, 96, 160, 255].repeat(64 * 64))
                .unwrap();
        }
        let saved: serde_json::Value = client
            .post(format!("{}/api/v1/skins?name=Fixture", bootstrap.base_url))
            .header(transport::CAPABILITY_HEADER, &bootstrap.capability)
            .header("content-type", "image/png")
            .body(png)
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        let key = saved["texture_key"].as_str().unwrap();
        let ticket: serde_json::Value = client
            .post(format!("{}/api/v1/transport/tickets", bootstrap.base_url))
            .header(transport::CAPABILITY_HEADER, &bootstrap.capability)
            .json(&serde_json::json!({"audience":"media"}))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        let grant = ticket["ticket"].as_str().unwrap();
        let file = client
            .get(format!(
                "{}/api/v1/skins/{key}/file?axial_ticket={grant}",
                bootstrap.base_url
            ))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap();
        assert_eq!(file.headers()["content-type"], "image/png");
        assert!(
            file.bytes()
                .await
                .unwrap()
                .starts_with(b"\x89PNG\r\n\x1a\n")
        );
        assert_eq!(
            client
                .get(format!(
                    "{}/api/v1/config?axial_ticket={grant}",
                    bootstrap.base_url
                ))
                .send()
                .await
                .unwrap()
                .status(),
            reqwest::StatusCode::UNAUTHORIZED
        );
        services.server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn real_queue_stream_uses_one_use_ticket_and_closes_on_shutdown() {
        let temporary = tempfile::tempdir().unwrap();
        let services = start_in_profile(
            temporary.path().join("replacement"),
            Some("http://localhost:1420"),
        )
        .await
        .unwrap();
        let bootstrap = services.server.bootstrap();
        let client = reqwest::Client::new();
        let browser_bootstrap: serde_json::Value = client
            .post(format!("{}/api/v1/transport/bootstrap", bootstrap.base_url))
            .header("origin", "http://localhost:1420")
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(browser_bootstrap["capability"], bootstrap.capability);
        let ticket: serde_json::Value = client
            .post(format!("{}/api/v1/transport/tickets", bootstrap.base_url))
            .header(transport::CAPABILITY_HEADER, &bootstrap.capability)
            .json(&serde_json::json!({"audience":"stream","target":"/api/v1/install/queue/events"}))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        let url = format!(
            "{}/api/v1/install/queue/events?axial_ticket={}",
            bootstrap.base_url,
            ticket["ticket"].as_str().unwrap()
        );
        let mut stream = client
            .get(&url)
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap();
        assert_eq!(stream.headers()["content-type"], "text/event-stream");
        let mut initial = Vec::new();
        tokio::time::timeout(Duration::from_secs(2), async {
            while !initial.windows(2).any(|window| window == b"\n\n") {
                initial.extend_from_slice(&stream.chunk().await.unwrap().unwrap());
            }
        })
        .await
        .unwrap();
        let initial = String::from_utf8(initial).unwrap();
        let data = initial
            .lines()
            .find_map(|line| {
                line.strip_prefix("data: ")
                    .or_else(|| line.strip_prefix("data:"))
            })
            .unwrap();
        let event: serde_json::Value = serde_json::from_str(data).unwrap();
        assert_eq!(event["revision"], services.installs.snapshot().revision);
        assert_eq!(event["value"]["active"], serde_json::Value::Null);
        assert_eq!(
            client.get(&url).send().await.unwrap().status(),
            reqwest::StatusCode::UNAUTHORIZED
        );
        services.server.shutdown().await.unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            while stream.chunk().await.unwrap().is_some() {}
        })
        .await
        .unwrap();
    }
}
