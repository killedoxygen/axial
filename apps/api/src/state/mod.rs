mod accounts;
mod auth_logins;
mod auth_persistence;
pub mod benchmark_suite_drivers;
pub mod benchmark_suites;
mod config;
pub mod contracts;
pub mod failure_memory;
mod installed_versions;
mod installs;
mod instance_lifecycle;
mod instance_registry;
mod integrity_activity;
mod java_probe_failures;
mod journals;
mod known_good;
mod known_good_rebuilds;
mod known_good_tier2;
pub(crate) mod launch_reports;
mod lifecycle;
pub mod ownership;
mod performance_managed;
pub mod performance_operations;
mod performance_rules;
pub mod presence;
mod reconciliation;
mod registered_artifact_findings;
mod remote_flags;
mod sessions;
mod shutdown;
pub mod skins;

use axial_config::{
    AppConfig, ConfigStore as StartupConfigStore, ConfigStoreError, INSTANCE_REGISTRY_MAX_ENTRIES,
    InstanceStore as StartupInstanceStore, InstanceStoreError, find_flag, generate_instance_id,
    is_canonical_instance_id,
};
pub use axial_launcher::{LaunchEvent, LaunchLogEvent, LaunchSessionRecord, LaunchStatusEvent};
use axial_minecraft::ManagedRuntimeCache;
pub use axial_minecraft::download::DownloadProgress;
use axial_performance::PerformanceManager;
use std::path::{Path, PathBuf};
use std::sync::Arc;
#[cfg(test)]
use std::sync::RwLock;
use tokio::sync::broadcast;

use crate::observability::telemetry::TelemetryHub;

const STARTUP_WARNING_LIMIT: usize = 8;
const STARTUP_WARNING_MAX_CHARS: usize = 240;

pub use accounts::{
    LauncherAccountKind, LauncherAccountRecord, LauncherAccountStore, microsoft_account_id,
    offline_account_id,
};
pub use auth_logins::{
    ActiveMinecraftAccountState, ActiveMsaTokenState, AuthLoginAccountState,
    AuthLoginMinecraftAccount, AuthLoginMinecraftCape, AuthLoginMinecraftProfile,
    AuthLoginMinecraftSkin, AuthLoginMsaToken, AuthLoginStore, NewAuthLoginMinecraftAccount,
    NewAuthLoginMsaToken,
};
pub use config::AppConfigStore;
pub use failure_memory::GuardianFailureMemoryStore;
pub(crate) use installed_versions::{InstalledVersionsLookup, InstalledVersionsSnapshot};
pub(crate) use installs::InstallInitializationStatus;
pub use installs::{
    ActiveQueuedInstallEntry, InstallProgressRecord, InstallQueueEnqueueOutcome,
    InstallQueuePlacement, InstallQueueSnapshot, InstallQueueSpec, InstallSnapshot, InstallStore,
    QueuedInstallEntry,
};
pub use instance_registry::AppInstanceStore;
pub(crate) use instance_registry::{InstanceUpdate, new_instance};
pub(crate) use instance_registry::{ensure_instance_layout, instance_not_found_error};
pub(crate) use integrity_activity::{
    IdleSweepAuthority, IdleSweepCancellation, IdleSweepReservation, IdleSweepReserveError,
    IdleSweepSettlement, IdleSweepSettlementOwner, IdleSweepTerminal, IntegrityActivityClosed,
    IntegrityForegroundLease, IntegrityForegroundRegistration, IntegrityIdleEpoch,
    IntegrityIdleSnapshot,
};
pub(crate) use java_probe_failures::{
    JavaProbeFailureCache, JavaProbeFailureClaim, JavaProbeFailureKey, JavaProbeFailureKind,
    JavaProbeFailureOwner,
};
pub(crate) use journals::{
    MAX_OPERATION_JOURNAL_DIAGNOSES, MAX_OPERATION_JOURNAL_STEP_FACTS,
    OperationJournalReconciliation, operation_journal_completed_step_is_visible,
    operation_journal_plan_is_visible, operation_journal_terminal_is_visible,
};
pub use journals::{OperationJournalStore, OperationJournalStoreError};
pub(crate) use known_good_rebuilds::KnownGoodRebuildError;
pub(crate) use known_good_tier2::KnownGoodTier2Ticket;
pub(crate) use lifecycle::{
    AppLifecycle, LifecycleAdmissionError, ProducerLease, RequestLease, RequestProducerHandoff,
};
#[cfg(test)]
pub(crate) use lifecycle::{AppLifecyclePhase, LifecycleQuiesceError};
pub(crate) use performance_managed::{
    AppManagedCompositionAdmission, ManagedCompositionCloseError, ManagedInspectionError,
    ManagedInstanceAdmissionError,
};
pub use performance_rules::AppPerformanceStore;
pub(crate) use reconciliation::{
    ASSETS_COMPONENT_REBUILD_STEP, COMPONENT_QUARANTINE_STEP, COMPONENT_REBUILD_START_STEP,
    LIBRARIES_COMPONENT_REBUILD_STEP, REGISTERED_ARTIFACT_COMPONENT_REBUILD_FAILURE_POINT,
    RUNTIME_COMPONENT_REBUILD_STEP, ReconciliationAttemptReservation,
    ReconciliationEvidenceRejection, RegisteredArtifactFailedRepair,
    RegisteredArtifactRecoveryEntry, RegisteredAssetsComponentRebuildEffect,
    RegisteredComponentRebuildAdmission, RegisteredLibrariesComponentRebuildEffect,
    RegisteredManagedArtifactCommitPostcheck, RegisteredManagedArtifactComponentCompletion,
    RegisteredManagedArtifactComponentEffectAdmission,
    RegisteredManagedArtifactComponentSettlement, RegisteredReconciliationAuthority,
    RegisteredVersionBundleComponentRebuildEffect, RegisteredWholeInstanceEffectAdmission,
    RegisteredWholeInstancePreparationError, RegisteredWholeInstanceRematerializationAvailability,
    VERSION_BUNDLE_COMPONENT_REBUILD_STEP, commit_reconciliation_memory, component_rebuild_journal,
    reconciliation_attempt_key, reconciliation_instance_target, reconciliation_journal_attempt,
    reconciliation_memory_entry, record_guardian_repair_refusal,
    record_reconciliation_journal_failure, record_reconciliation_journal_success,
    reserve_reconciliation_attempt, settle_reconciliation_memory, validate_reconciliation_memory,
};
pub use registered_artifact_findings::RegisteredArtifactRepairCandidate;
pub(crate) use registered_artifact_findings::{
    RegisteredArtifactCondition, RegisteredArtifactFindings, RegisteredArtifactObservation,
    RegisteredArtifactRepairAdmission, RegisteredArtifactRepairEffect,
    RegisteredArtifactRepairMemoryReceipt, RegisteredArtifactRepairPlanRef,
};
#[cfg(test)]
pub(crate) use registered_artifact_findings::{
    RegisteredArtifactRepairAuthorizationRejection, registered_artifact_target_for_test,
};
pub(crate) use remote_flags::{
    RemoteFlagRefreshOutcome, RemoteFlagStore, ResolvedFlagSource, resolve_flag,
};
pub(crate) use sessions::{LaunchFailureTermination, LaunchFailureTerminationErrorClass};
pub use sessions::{SessionAdmissionError, SessionStopError, SessionStore, StartupOutcome};
use shutdown::AppShutdownCoordinator;
pub use shutdown::{AppShutdownError, AppShutdownStep};

#[derive(Clone)]
pub struct AppState {
    app_name: String,
    version: String,
    config: Arc<AppConfigStore>,
    managed_runtime_cache: ManagedRuntimeCache,
    instances: Arc<AppInstanceStore>,
    accounts: Arc<LauncherAccountStore>,
    auth_logins: Arc<AuthLoginStore>,
    installs: Arc<InstallStore>,
    failure_memory: Arc<GuardianFailureMemoryStore>,
    journals: Arc<OperationJournalStore>,
    installed_versions: Arc<installed_versions::InstalledVersionsIndex>,
    known_good: Arc<known_good::KnownGoodInventoryStore>,
    known_good_rebuilds: Arc<known_good_rebuilds::KnownGoodRebuildFlights>,
    java_probe_failures: Arc<JavaProbeFailureCache>,
    sessions: Arc<SessionStore>,
    skins: Arc<skins::SavedSkinStore>,
    benchmark_suites: Arc<benchmark_suites::BenchmarkSuiteStore>,
    benchmark_suite_drivers: Arc<benchmark_suite_drivers::BenchmarkSuiteDriverStore>,
    performance_operations: Arc<performance_operations::PerformanceOperationStore>,
    performance: Arc<AppPerformanceStore>,
    telemetry: Arc<TelemetryHub>,
    remote_flags: Arc<RemoteFlagStore>,
    launch_reports: Arc<launch_reports::LaunchReportStore>,
    integrity_activity: integrity_activity::IntegrityActivityCoordinator,
    instance_lifecycle_gates: instance_lifecycle::InstanceLifecycleGates,
    lifecycle: AppLifecycle,
    shutdown_coordinator: AppShutdownCoordinator,
    startup_warnings: Arc<Vec<String>>,
    config_changes: Arc<broadcast::Sender<()>>,
    #[cfg(test)]
    auth_chain_client_override: Arc<RwLock<Option<crate::auth_chain::AuthChainClient>>>,
    frontend_dir: Arc<PathBuf>,
}

pub struct AppStateInit {
    pub app_name: String,
    pub version: String,
    pub config: Arc<StartupConfigStore>,
    pub instances: Arc<StartupInstanceStore>,
    pub installs: Arc<InstallStore>,
    pub sessions: Arc<SessionStore>,
    pub performance: Arc<PerformanceManager>,
    pub startup_warnings: Vec<String>,
    pub frontend_dir: PathBuf,
}

struct KnownGoodCandidateAdmission {
    _lifecycle: InstanceLifecycleLease,
    instance_id: String,
    version_id: String,
    created_at: String,
    library_root: PathBuf,
}

pub(crate) struct InstanceLifecycleLease {
    instance_id: String,
    owner: instance_lifecycle::InstanceLifecycleGates,
    _guard: Arc<tokio::sync::OwnedMutexGuard<()>>,
}

pub(crate) struct KnownGoodVerificationLease {
    owner: KnownGoodVerificationOwner,
    _lifecycle: InstanceLifecycleLease,
    instance_id: String,
    version_id: String,
    created_at: String,
    library_root: PathBuf,
    managed_runtime_cache: ManagedRuntimeCache,
    inventory: Arc<axial_minecraft::known_good::KnownGoodInventory>,
}

enum KnownGoodVerificationOwner {
    Foreground(IntegrityForegroundLease),
    IdleSweep(IdleSweepAuthority),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum KnownGoodVerificationUnavailable {
    InstanceNotRegistered,
    LibraryRootUnavailable,
    LiveAuthorityUnavailable,
    SweepAuthorityUnavailable,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("integrity foreground lease belongs to another application state")]
pub(crate) struct IntegrityForegroundOwnershipError;

pub(crate) struct ManagedLibrarySetupTarget {
    owner: Arc<AppConfigStore>,
    library_dir: PathBuf,
}

impl ManagedLibrarySetupTarget {
    pub(crate) fn library_dir(&self) -> &Path {
        &self.library_dir
    }
}

impl InstanceLifecycleLease {
    fn bind(
        instance_id: &str,
        owner: instance_lifecycle::InstanceLifecycleGates,
        guard: tokio::sync::OwnedMutexGuard<()>,
    ) -> Self {
        Self {
            instance_id: instance_id.to_string(),
            owner,
            _guard: Arc::new(guard),
        }
    }

    fn matches(&self, instance_id: &str) -> bool {
        self.instance_id == instance_id
    }

    pub(crate) fn retained(&self) -> Self {
        Self {
            instance_id: self.instance_id.clone(),
            owner: self.owner.clone(),
            _guard: self._guard.clone(),
        }
    }
}

impl KnownGoodVerificationLease {
    fn retained(&self) -> Self {
        Self {
            owner: match &self.owner {
                KnownGoodVerificationOwner::Foreground(foreground) => {
                    KnownGoodVerificationOwner::Foreground(foreground.retained())
                }
                KnownGoodVerificationOwner::IdleSweep(authority) => {
                    KnownGoodVerificationOwner::IdleSweep(authority.clone())
                }
            },
            _lifecycle: self._lifecycle.retained(),
            instance_id: self.instance_id.clone(),
            version_id: self.version_id.clone(),
            created_at: self.created_at.clone(),
            library_root: self.library_root.clone(),
            managed_runtime_cache: self.managed_runtime_cache.clone(),
            inventory: self.inventory.clone(),
        }
    }

    pub(crate) fn execution_parts(
        &self,
    ) -> (
        &str,
        &str,
        &str,
        &Path,
        &ManagedRuntimeCache,
        &axial_minecraft::known_good::KnownGoodInventory,
    ) {
        (
            &self.instance_id,
            &self.version_id,
            &self.created_at,
            &self.library_root,
            &self.managed_runtime_cache,
            &self.inventory,
        )
    }

    #[cfg(test)]
    pub(crate) fn exact_identity_for_test(&self) -> (&str, &str, &str, &Path) {
        (
            &self.instance_id,
            &self.version_id,
            &self.created_at,
            &self.library_root,
        )
    }
}

impl KnownGoodCandidateAdmission {
    fn revalidate(&self, state: &AppState) -> std::io::Result<bool> {
        if !matches_known_good_incarnation(
            state.instances.get(&self.instance_id).as_ref(),
            &self.instance_id,
            &self.version_id,
            &self.created_at,
        ) {
            return Ok(false);
        }
        require_matching_known_good_library_root(state.library_dir(), &self.library_root)
            .map(|root| root == self.library_root)
    }

    fn deactivate(&self, state: &AppState) {
        state.known_good.deactivate_exact(
            &self.instance_id,
            &self.version_id,
            &self.created_at,
            &self.library_root,
        );
    }
}

impl AppState {
    #[cfg(test)]
    pub fn new(init: AppStateInit) -> Self {
        let config =
            Arc::new(AppConfigStore::claim(&init.config).unwrap_or_else(|error| {
                panic!("failed to initialize config persistence: {error}")
            }));
        let telemetry = Arc::new(TelemetryHub::from_env(config.clone()));
        assert!(
            !config.current().telemetry_enabled
                || !telemetry.export_configured()
                || !config.current().telemetry_install_id.is_empty(),
            "synchronous test state requires a committed telemetry install id"
        );
        Self::new_with_telemetry_inner(
            init,
            config,
            telemetry,
            Arc::new(AuthLoginStore::new()),
            Arc::new(RemoteFlagStore::default()),
            ManagedRuntimeCache::isolated_for_test()
                .expect("failed to create isolated managed runtime cache"),
        )
        .unwrap_or_else(|error| {
            panic!("failed to initialize known-good inventory persistence: {error}")
        })
    }

    pub async fn load(mut init: AppStateInit) -> std::io::Result<Self> {
        let config =
            Arc::new(AppConfigStore::claim(&init.config).unwrap_or_else(|error| {
                panic!("failed to initialize config persistence: {error}")
            }));
        let telemetry = Arc::new(TelemetryHub::from_env(config.clone()));
        let telemetry_identity_required = config.current().telemetry_enabled
            && telemetry.export_configured()
            && config.current().telemetry_install_id.is_empty();
        if telemetry_identity_required
            && config
                .mutate(|_| Ok(()), true, Arc::new(|_, _| {}))
                .await
                .is_err()
        {
            init.startup_warnings.push(
                "Axial could not persist telemetry identity; telemetry remains disabled until settings persistence recovers."
                    .to_string(),
            );
        }
        let remote_flags_config_dir = init.config.paths().config_dir.clone();
        let (auth_logins, remote_flags) = tokio::join!(
            AuthLoginStore::load_from_secure_store(),
            RemoteFlagStore::load_from_config_dir(remote_flags_config_dir),
        );
        #[cfg(not(test))]
        let managed_runtime_cache = ManagedRuntimeCache::canonical()?;
        #[cfg(test)]
        let managed_runtime_cache = ManagedRuntimeCache::isolated_for_test()?;
        let state = tokio::task::spawn_blocking(move || {
            Self::new_with_telemetry_inner(
                init,
                config,
                telemetry,
                Arc::new(auth_logins),
                Arc::new(remote_flags),
                managed_runtime_cache,
            )
        })
        .await
        .map_err(|_| std::io::Error::other("persisted state startup task stopped"))??;
        if state.known_good.retry_retirements().await.is_err() {
            tracing::warn!("known-good restart cleanup remains pending");
        }
        state.reconcile_reconciliation_startup().await?;
        Ok(state)
    }

    #[cfg(test)]
    pub(crate) fn new_with_telemetry(init: AppStateInit, telemetry: Arc<TelemetryHub>) -> Self {
        let config =
            Arc::new(AppConfigStore::claim(&init.config).unwrap_or_else(|error| {
                panic!("failed to initialize config persistence: {error}")
            }));
        telemetry.replace_config_source(config.clone());
        Self::new_with_telemetry_inner(
            init,
            config,
            telemetry,
            Arc::new(AuthLoginStore::new()),
            Arc::new(RemoteFlagStore::default()),
            ManagedRuntimeCache::isolated_for_test()
                .expect("failed to create isolated managed runtime cache"),
        )
        .unwrap_or_else(|error| {
            panic!("failed to initialize known-good inventory persistence: {error}")
        })
    }

    #[cfg(test)]
    pub(crate) fn with_operation_stores(
        mut self,
        journals: Arc<OperationJournalStore>,
        performance_operations: Arc<performance_operations::PerformanceOperationStore>,
    ) -> Self {
        self.journals = journals;
        self.performance_operations = performance_operations;
        self
    }

    #[cfg(test)]
    pub(crate) fn with_reconciliation_stores(
        mut self,
        journals: Arc<OperationJournalStore>,
        failure_memory: Arc<GuardianFailureMemoryStore>,
    ) -> Self {
        self.journals = journals;
        self.failure_memory = failure_memory;
        self
    }

    #[cfg(test)]
    pub(crate) fn with_accounts(mut self, accounts: Arc<LauncherAccountStore>) -> Self {
        self.accounts = accounts;
        self
    }

    #[cfg(test)]
    pub(crate) fn with_benchmark_suites(
        mut self,
        benchmark_suites: Arc<benchmark_suites::BenchmarkSuiteStore>,
    ) -> Self {
        self.launch_reports
            .bind_proof_retention(benchmark_suites.proof_retention_handle());
        self.benchmark_suites = benchmark_suites;
        self
    }

    fn new_with_telemetry_inner(
        init: AppStateInit,
        config: Arc<AppConfigStore>,
        telemetry: Arc<TelemetryHub>,
        auth_logins: Arc<AuthLoginStore>,
        remote_flags: Arc<RemoteFlagStore>,
        managed_runtime_cache: ManagedRuntimeCache,
    ) -> std::io::Result<Self> {
        let instance_registry_authoritative = init.instances.mutation_allowed();
        let instances = Arc::new(AppInstanceStore::claim(&init.instances).unwrap_or_else(
            |error| panic!("failed to initialize instance registry persistence: {error}"),
        ));
        let instance_lifecycle_gates = instance_lifecycle::InstanceLifecycleGates::default();
        let performance = Arc::new(
            AppPerformanceStore::claim(
                init.performance,
                &config.paths().config_dir,
                &instances.paths().instances_dir,
                instance_lifecycle_gates.clone(),
            )
            .unwrap_or_else(|error| {
                panic!("failed to initialize performance rules persistence: {error}")
            }),
        );
        let benchmark_suite_retention_claims =
            benchmark_suites::BenchmarkSuiteRetentionClaims::default();
        let benchmark_suite_drivers =
            benchmark_suite_drivers::BenchmarkSuiteDriverStore::prepare_load_from_paths(
                config.paths(),
                benchmark_suite_retention_claims.clone(),
            );
        let benchmark_suites = Arc::new(benchmark_suites::BenchmarkSuiteStore::load_from_paths(
            config.paths(),
            benchmark_suite_retention_claims,
        ));
        let launch_reports = Arc::new(launch_reports::LaunchReportStore::load_from_paths(
            config.paths(),
            benchmark_suites.proof_retention_handle(),
        ));
        let benchmark_suite_drivers =
            Arc::new(benchmark_suite_drivers.bind(benchmark_suites.retention_handle()));
        let performance_operations = Arc::new(
            performance_operations::PerformanceOperationStore::load_from_paths(config.paths()),
        );
        let skins = Arc::new(skins::SavedSkinStore::load_from_paths(config.paths()));
        let accounts = Arc::new(LauncherAccountStore::load_from_paths(config.paths()));
        let failure_memory = Arc::new(
            GuardianFailureMemoryStore::try_load_from_paths(config.paths()).map_err(|error| {
                std::io::Error::other(format!("failed to load Guardian failure memory: {error}"))
            })?,
        );
        let journals = Arc::new(
            OperationJournalStore::try_load_from_paths(config.paths()).map_err(|error| {
                std::io::Error::other(format!("failed to load operation journals: {error}"))
            })?,
        );
        let known_good = Arc::new(known_good::KnownGoodInventoryStore::claim(config.paths())?);
        if instance_registry_authoritative {
            known_good.discover_absent_snapshot_obligations(
                instances.list().into_iter().map(|instance| instance.id),
            )?;
        }
        let (config_changes, _) = broadcast::channel(32);

        Ok(Self {
            app_name: init.app_name,
            version: init.version,
            config,
            managed_runtime_cache,
            instances,
            accounts,
            auth_logins,
            installs: init.installs,
            failure_memory,
            journals,
            installed_versions: Arc::new(installed_versions::InstalledVersionsIndex::default()),
            known_good,
            known_good_rebuilds: Arc::new(known_good_rebuilds::KnownGoodRebuildFlights::default()),
            java_probe_failures: Arc::new(JavaProbeFailureCache::default()),
            sessions: init.sessions,
            skins,
            benchmark_suites,
            benchmark_suite_drivers,
            performance_operations,
            performance,
            telemetry,
            remote_flags,
            launch_reports,
            integrity_activity: integrity_activity::IntegrityActivityCoordinator::new(),
            instance_lifecycle_gates,
            lifecycle: AppLifecycle::new(),
            shutdown_coordinator: AppShutdownCoordinator::new(),
            startup_warnings: Arc::new(bound_startup_warnings(init.startup_warnings)),
            config_changes: Arc::new(config_changes),
            #[cfg(test)]
            auth_chain_client_override: Arc::new(RwLock::new(None)),
            frontend_dir: Arc::new(init.frontend_dir),
        })
    }

    pub fn app_name(&self) -> &str {
        &self.app_name
    }

    pub fn version(&self) -> &str {
        &self.version
    }

    pub fn config(&self) -> &Arc<AppConfigStore> {
        &self.config
    }

    pub(crate) fn managed_runtime_cache(&self) -> &ManagedRuntimeCache {
        &self.managed_runtime_cache
    }

    pub fn instances(&self) -> &Arc<AppInstanceStore> {
        &self.instances
    }

    pub fn accounts(&self) -> &Arc<LauncherAccountStore> {
        &self.accounts
    }

    pub fn sessions(&self) -> &Arc<SessionStore> {
        &self.sessions
    }

    pub fn skins(&self) -> &Arc<skins::SavedSkinStore> {
        &self.skins
    }

    pub fn auth_logins(&self) -> &Arc<AuthLoginStore> {
        &self.auth_logins
    }

    pub fn benchmark_suite_drivers(
        &self,
    ) -> &Arc<benchmark_suite_drivers::BenchmarkSuiteDriverStore> {
        &self.benchmark_suite_drivers
    }

    pub fn benchmark_suites(&self) -> &Arc<benchmark_suites::BenchmarkSuiteStore> {
        &self.benchmark_suites
    }

    pub fn performance_operations(
        &self,
    ) -> &Arc<performance_operations::PerformanceOperationStore> {
        &self.performance_operations
    }

    pub fn installs(&self) -> &Arc<InstallStore> {
        &self.installs
    }

    pub fn failure_memory(&self) -> &Arc<GuardianFailureMemoryStore> {
        &self.failure_memory
    }

    pub fn journals(&self) -> &Arc<OperationJournalStore> {
        &self.journals
    }

    pub(crate) async fn installed_versions_snapshot(
        &self,
        producer: &ProducerLease,
    ) -> Option<InstalledVersionsLookup> {
        let foreground = self
            .register_integrity_foreground()
            .ok()?
            .wait_for_settlement()
            .await;
        self.installed_versions_snapshot_with_foreground(producer, foreground)
            .await
    }

    pub(crate) async fn installed_versions_snapshot_with_foreground(
        &self,
        producer: &ProducerLease,
        foreground: IntegrityForegroundLease,
    ) -> Option<InstalledVersionsLookup> {
        self.validate_integrity_foreground(&foreground).ok()?;
        let library_dir = self.library_dir().map(PathBuf::from)?;
        Some(
            self.installed_versions
                .lookup(library_dir, producer, foreground)
                .await,
        )
    }

    pub(crate) fn invalidate_installed_versions(&self) {
        self.installed_versions.invalidate();
    }

    #[cfg(test)]
    pub(crate) fn installed_versions_walk_count(&self) -> usize {
        self.installed_versions.walk_count()
    }

    pub(crate) fn java_probe_failures(&self) -> &Arc<JavaProbeFailureCache> {
        &self.java_probe_failures
    }

    pub(crate) async fn accept_known_good_install_receipt(
        &self,
        foreground: &IntegrityForegroundLease,
        installed_library_root: &Path,
        receipt: axial_minecraft::known_good::KnownGoodInstallReceipt,
    ) -> std::io::Result<()> {
        self.validate_integrity_foreground(foreground)
            .map_err(|_| foreign_integrity_foreground_error())?;
        self.activate_known_good_source(
            foreground,
            installed_library_root,
            receipt.into_activation_source(),
        )
        .await
    }

    async fn activate_known_good_source(
        &self,
        foreground: &IntegrityForegroundLease,
        installed_library_root: &Path,
        source: axial_minecraft::known_good::KnownGoodActivationSource,
    ) -> std::io::Result<()> {
        self.validate_integrity_foreground(foreground)
            .map_err(|_| foreign_integrity_foreground_error())?;
        let installed_library_root =
            require_matching_known_good_library_root(self.library_dir(), installed_library_root)?;
        let (version_id, inventory) = source.into_parts();
        let candidates = self
            .instances
            .list()
            .into_iter()
            .filter(|instance| {
                matches_known_good_incarnation(
                    Some(instance),
                    &instance.id,
                    &version_id,
                    &instance.created_at,
                )
            })
            .map(|instance| (instance.id, instance.created_at))
            .take(INSTANCE_REGISTRY_MAX_ENTRIES + 1)
            .collect::<Vec<_>>();
        if candidates.len() > INSTANCE_REGISTRY_MAX_ENTRIES {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "known-good activation candidate count exceeds the instance registry limit",
            ));
        }
        let inventory = Arc::new(inventory);
        let version_id = version_id.as_str();
        let installed_library_root = installed_library_root.as_path();
        complete_independent_known_good_fanout(candidates, |(instance_id, created_at)| {
            let inventory = inventory.clone();
            async move {
                self.reconcile_known_good_instance(
                    foreground,
                    &instance_id,
                    version_id,
                    &created_at,
                    installed_library_root,
                    inventory,
                )
                .await
            }
        })
        .await
    }

    async fn reconcile_known_good_instance(
        &self,
        foreground: &IntegrityForegroundLease,
        instance_id: &str,
        version_id: &str,
        created_at: &str,
        installed_library_root: &Path,
        inventory: Arc<axial_minecraft::known_good::KnownGoodInventory>,
    ) -> std::io::Result<()> {
        let admission = match self
            .admit_known_good_candidate(
                foreground,
                instance_id,
                version_id,
                created_at,
                installed_library_root,
            )
            .await
        {
            Ok(Some(admission)) => admission,
            Ok(None) => return Ok(()),
            Err(error) => return Err(error),
        };

        if let Err(error) = self
            .known_good
            .reconcile(
                &admission.instance_id,
                &admission.version_id,
                &admission.created_at,
                &admission.library_root,
                inventory,
            )
            .await
        {
            admission.deactivate(self);
            return Err(error);
        }

        match admission.revalidate(self) {
            Ok(true) => {}
            Ok(false) => {
                admission.deactivate(self);
                return Ok(());
            }
            Err(error) => {
                admission.deactivate(self);
                return Err(error);
            }
        }
        if self
            .known_good
            .active_inventory(
                &admission.instance_id,
                &admission.version_id,
                &admission.created_at,
                &admission.library_root,
            )
            .is_none()
        {
            admission.deactivate(self);
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotConnected,
                "known-good live authority was not activated",
            ));
        }
        Ok(())
    }

    async fn admit_known_good_candidate(
        &self,
        foreground: &IntegrityForegroundLease,
        instance_id: &str,
        version_id: &str,
        created_at: &str,
        installed_library_root: &Path,
    ) -> std::io::Result<Option<KnownGoodCandidateAdmission>> {
        let lifecycle = self
            .acquire_integrity_instance_lifecycle(foreground, instance_id)
            .await
            .map_err(|_| foreign_integrity_foreground_error())?;
        if !matches_known_good_incarnation(
            self.instances.get(instance_id).as_ref(),
            instance_id,
            version_id,
            created_at,
        ) {
            self.known_good.deactivate_exact(
                instance_id,
                version_id,
                created_at,
                installed_library_root,
            );
            return Ok(None);
        }
        let library_root = match require_matching_known_good_library_root(
            self.library_dir(),
            installed_library_root,
        ) {
            Ok(root) => root,
            Err(error) => {
                self.known_good.deactivate_exact(
                    instance_id,
                    version_id,
                    created_at,
                    installed_library_root,
                );
                return Err(error);
            }
        };
        Ok(Some(KnownGoodCandidateAdmission {
            _lifecycle: lifecycle,
            instance_id: instance_id.to_string(),
            version_id: version_id.to_string(),
            created_at: created_at.to_string(),
            library_root,
        }))
    }

    pub fn performance(&self) -> &Arc<AppPerformanceStore> {
        &self.performance
    }

    pub(crate) async fn refresh_performance_rules(
        &self,
    ) -> Result<axial_performance::PerformanceRulesStatus, axial_performance::RulesRefreshError>
    {
        let performance = self.performance.clone();
        let gate = performance.acquire_refresh().await?;
        let (completed_tx, completed_rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let result = performance.refresh_with_gate(gate).await;
            let _ = completed_tx.send(result);
        });
        completed_rx.await.map_err(|_| {
            axial_performance::RulesRefreshError::Cache(std::io::Error::other(
                "performance rules refresh owner stopped before reporting completion",
            ))
        })?
    }

    pub(crate) async fn close_performance_rules(
        &self,
    ) -> Result<(), axial_performance::RulesRefreshError> {
        self.performance.close().await
    }

    pub fn telemetry(&self) -> &Arc<TelemetryHub> {
        &self.telemetry
    }

    pub(crate) fn remote_flags(&self) -> &Arc<RemoteFlagStore> {
        &self.remote_flags
    }

    pub(crate) fn launch_reports(&self) -> &Arc<launch_reports::LaunchReportStore> {
        &self.launch_reports
    }

    pub(crate) fn try_admit_request(&self) -> Result<RequestLease, LifecycleAdmissionError> {
        self.lifecycle.try_admit_request()
    }

    pub(crate) fn try_claim_producer(&self) -> Result<ProducerLease, LifecycleAdmissionError> {
        self.lifecycle.try_claim_producer()
    }

    pub(crate) fn subscribe_shutdown(&self) -> tokio::sync::watch::Receiver<bool> {
        self.lifecycle.subscribe_shutdown()
    }

    pub(crate) fn subscribe_integrity_idle(
        &self,
    ) -> tokio::sync::watch::Receiver<IntegrityIdleSnapshot> {
        self.integrity_activity.subscribe_idle()
    }

    pub(crate) fn register_integrity_foreground(
        &self,
    ) -> Result<IntegrityForegroundRegistration, IntegrityActivityClosed> {
        self.integrity_activity.register_foreground()
    }

    pub(crate) fn try_reserve_idle_sweep(
        &self,
        expected_epoch: IntegrityIdleEpoch,
        producer: ProducerLease,
    ) -> Result<IdleSweepReservation, IdleSweepReserveError> {
        self.integrity_activity
            .try_reserve_idle_sweep(expected_epoch, producer)
    }

    pub(crate) fn idle_sweep_authority_is_current(&self, authority: &IdleSweepAuthority) -> bool {
        self.integrity_activity
            .owns_current_sweep_authority(authority)
    }

    pub(crate) fn idle_sweep_authority_is_active(&self, authority: &IdleSweepAuthority) -> bool {
        self.integrity_activity
            .owns_active_sweep_authority(authority)
    }

    #[cfg(test)]
    pub(crate) async fn quiesce(&self) -> Result<(), LifecycleQuiesceError> {
        self.lifecycle.begin_quiesce();
        self.lifecycle.wait_for_shutdown_started().await?;
        self.integrity_activity.begin_shutdown();
        self.lifecycle.wait_for_quiesced().await
    }

    pub async fn shutdown(&self) -> Result<(), AppShutdownError> {
        self.lifecycle.begin_quiesce();
        self.shutdown_coordinator.start(self.clone()).wait().await
    }

    #[cfg(test)]
    pub(crate) fn lifecycle_phase(&self) -> AppLifecyclePhase {
        self.lifecycle.phase()
    }

    pub(crate) fn remote_flag_identity_for(&self, config: &AppConfig) -> Option<String> {
        self.telemetry.export_identity_for_config(config)
    }

    pub fn startup_warnings(&self) -> Vec<String> {
        self.startup_warnings.as_ref().clone()
    }

    pub fn library_dir(&self) -> Option<String> {
        let library_dir = self.config.current().library_dir;
        (!library_dir.trim().is_empty()).then_some(library_dir)
    }

    #[cfg(test)]
    pub fn set_library_dir_for_test(&self, value: String) {
        let mut config = self.config.current();
        config.library_dir = value;
        self.replace_config_for_test(config);
    }

    #[cfg(test)]
    pub(crate) fn replace_config_for_test(&self, config: AppConfig) {
        let previous = self.config.current();
        self.config
            .replace_for_test(config)
            .expect("test config replacement must remain valid");
        let current = self.config.current();
        self.config_commit_observer()(previous, current);
    }

    pub async fn mutate_config<Mutation>(
        &self,
        mutation: Mutation,
    ) -> Result<AppConfig, ConfigStoreError>
    where
        Mutation: FnOnce(&mut AppConfig) -> Result<(), ConfigStoreError> + Send + 'static,
    {
        let config = self.config.clone();
        let gate = config.acquire_mutation().await?;
        let export_configured = self.telemetry.export_configured();
        let observer = self.config_commit_observer();
        let (completed_tx, completed_rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let result = config
                .mutate_with_gate(mutation, export_configured, observer, gate)
                .await;
            let _ = completed_tx.send(result);
        });
        completed_rx.await.map_err(|_| {
            ConfigStoreError::Persistence(std::io::Error::other(
                "application config mutation owner stopped before reporting completion",
            ))
        })?
    }

    pub(crate) async fn close_config(&self) -> Result<(), ConfigStoreError> {
        self.config.close(self.config_commit_observer()).await
    }

    pub(crate) fn managed_library_setup_target(
        &self,
        foreground: &IntegrityForegroundLease,
    ) -> Result<ManagedLibrarySetupTarget, ConfigStoreError> {
        self.validate_integrity_foreground(foreground)
            .map_err(|_| ConfigStoreError::Persistence(foreign_integrity_foreground_error()))?;
        Ok(ManagedLibrarySetupTarget {
            owner: self.config.clone(),
            library_dir: self.config.paths().library_dir.clone(),
        })
    }

    pub(crate) async fn commit_managed_library_setup(
        &self,
        foreground: &IntegrityForegroundLease,
        target: &ManagedLibrarySetupTarget,
    ) -> Result<AppConfig, ConfigStoreError> {
        self.validate_integrity_foreground(foreground)
            .map_err(|_| ConfigStoreError::Persistence(foreign_integrity_foreground_error()))?;
        if !Arc::ptr_eq(&target.owner, &self.config)
            || target.library_dir != self.config.paths().library_dir
        {
            return Err(ConfigStoreError::Persistence(
                foreign_integrity_foreground_error(),
            ));
        }
        let gate = self.config.acquire_mutation().await?;
        let library_dir = target.library_dir.to_string_lossy().into_owned();
        self.config
            .mutate_with_gate(
                move |latest| {
                    latest.library_dir = library_dir;
                    latest.library_mode = "managed".to_string();
                    Ok(())
                },
                self.telemetry.export_configured(),
                self.config_commit_observer(),
                gate,
            )
            .await
    }

    pub(crate) async fn create_instance(
        &self,
        foreground: &IntegrityForegroundLease,
        instance: axial_config::Instance,
        library_dir: Option<PathBuf>,
    ) -> Result<axial_config::Instance, InstanceStoreError> {
        self.validate_integrity_foreground(foreground)
            .map_err(|_| InstanceStoreError::Persistence(foreign_integrity_foreground_error()))?;
        let _lifecycle = self
            .acquire_integrity_instance_lifecycle(foreground, &instance.id)
            .await
            .map_err(|_| InstanceStoreError::Persistence(foreign_integrity_foreground_error()))?;
        let instances = self.instances.clone();
        let gate = instances.acquire_mutation().await?;
        instances
            .create_with_gate(instance, library_dir, gate)
            .await
    }

    pub(crate) async fn duplicate_instance(
        &self,
        foreground: &IntegrityForegroundLease,
        source_id: String,
        requested_name: Option<String>,
        library_dir: Option<PathBuf>,
    ) -> Result<axial_config::Instance, InstanceStoreError> {
        self.validate_integrity_foreground(foreground)
            .map_err(|_| InstanceStoreError::Persistence(foreign_integrity_foreground_error()))?;
        let target_id = loop {
            let candidate = generate_instance_id();
            if candidate != source_id && self.instances.get(&candidate).is_none() {
                break candidate;
            }
        };
        let (first_id, second_id) = if source_id < target_id {
            (&source_id, &target_id)
        } else {
            (&target_id, &source_id)
        };
        let _first_lifecycle = self
            .acquire_integrity_instance_lifecycle(foreground, first_id)
            .await
            .map_err(|_| InstanceStoreError::Persistence(foreign_integrity_foreground_error()))?;
        let _second_lifecycle = self
            .acquire_integrity_instance_lifecycle(foreground, second_id)
            .await
            .map_err(|_| InstanceStoreError::Persistence(foreign_integrity_foreground_error()))?;
        let instances = self.instances.clone();
        let gate = instances.acquire_mutation().await?;
        instances
            .duplicate_with_gate(source_id, target_id, requested_name, library_dir, gate)
            .await
    }

    pub(crate) async fn delete_instance(
        &self,
        foreground: &IntegrityForegroundLease,
        instance_id: String,
        delete_files: bool,
    ) -> Result<(), InstanceStoreError> {
        self.validate_integrity_foreground(foreground)
            .map_err(|_| InstanceStoreError::Persistence(foreign_integrity_foreground_error()))?;
        if self.sessions.has_active_instance(&instance_id).await {
            return Err(InstanceStoreError::Persistence(std::io::Error::new(
                std::io::ErrorKind::WouldBlock,
                "cannot delete a running instance; stop the game first",
            )));
        }
        let lifecycle = self
            .acquire_integrity_instance_lifecycle(foreground, &instance_id)
            .await
            .map_err(|_| InstanceStoreError::Persistence(foreign_integrity_foreground_error()))?;
        if self.sessions.has_active_instance(&instance_id).await {
            return Err(InstanceStoreError::Persistence(std::io::Error::new(
                std::io::ErrorKind::WouldBlock,
                "cannot delete a running instance; stop the game first",
            )));
        }
        if self.instances.get(&instance_id).is_none() {
            return Err(instance_not_found_error());
        }
        let retirement = self
            .performance
            .retire_managed(&instance_id)
            .await
            .map_err(|error| {
                InstanceStoreError::Persistence(std::io::Error::other(error.to_string()))
            })?;
        let known_good_retirement = self
            .known_good
            .reserve_retirement(&instance_id)
            .map_err(InstanceStoreError::Persistence)?;
        let instances = self.instances.clone();
        let _lifecycle = lifecycle;
        let retained_instance_id = instance_id.clone();
        let result = match instances.acquire_mutation().await {
            Ok(gate) => {
                instances
                    .delete_with_gate(instance_id, delete_files, gate)
                    .await
            }
            Err(error) => Err(error),
        };
        if instances.get(&retained_instance_id).is_none() {
            retirement.commit();
            if known_good_retirement.commit().await.is_err() {
                tracing::warn!(
                    instance_id = retained_instance_id,
                    "known-good retirement cleanup was retained for retry"
                );
            }
        } else if result.is_ok() {
            return Err(InstanceStoreError::Persistence(std::io::Error::other(
                "instance registry reported successful deletion without removing the instance",
            )));
        }
        result
    }

    pub(crate) async fn update_instance(
        &self,
        foreground: &IntegrityForegroundLease,
        instance_id: String,
        update: InstanceUpdate,
    ) -> Result<axial_config::Instance, InstanceStoreError> {
        self.validate_integrity_foreground(foreground)
            .map_err(|_| InstanceStoreError::Persistence(foreign_integrity_foreground_error()))?;
        let _lifecycle = self
            .acquire_integrity_instance_lifecycle(foreground, &instance_id)
            .await
            .map_err(|_| InstanceStoreError::Persistence(foreign_integrity_foreground_error()))?;
        let instances = self.instances.clone();
        let gate = instances.acquire_mutation().await?;
        instances.update_with_gate(instance_id, update, gate).await
    }

    pub(crate) async fn record_successful_launch_metadata(
        &self,
        foreground: &IntegrityForegroundLease,
        instance_id: String,
        last_played_at: String,
    ) -> Result<(), InstanceStoreError> {
        self.validate_integrity_foreground(foreground)
            .map_err(|_| InstanceStoreError::Persistence(foreign_integrity_foreground_error()))?;
        let _lifecycle = self
            .acquire_integrity_instance_lifecycle(foreground, &instance_id)
            .await
            .map_err(|_| InstanceStoreError::Persistence(foreign_integrity_foreground_error()))?;
        let instances = self.instances.clone();
        let gate = instances.acquire_mutation().await?;
        instances
            .record_successful_launch_with_gate(instance_id, last_played_at, gate)
            .await
    }

    pub(crate) async fn acquire_integrity_instance_lifecycle(
        &self,
        foreground: &IntegrityForegroundLease,
        instance_id: &str,
    ) -> Result<InstanceLifecycleLease, IntegrityForegroundOwnershipError> {
        self.validate_integrity_foreground(foreground)?;
        Ok(self.acquire_instance_lifecycle(instance_id).await)
    }

    fn validate_integrity_foreground(
        &self,
        foreground: &IntegrityForegroundLease,
    ) -> Result<(), IntegrityForegroundOwnershipError> {
        self.integrity_activity
            .owns_foreground(foreground)
            .then_some(())
            .ok_or(IntegrityForegroundOwnershipError)
    }

    #[cfg(not(test))]
    async fn acquire_instance_lifecycle(&self, instance_id: &str) -> InstanceLifecycleLease {
        InstanceLifecycleLease::bind(
            instance_id,
            self.instance_lifecycle_gates.clone(),
            self.instance_lifecycle_gates.acquire(instance_id).await,
        )
    }

    #[cfg(test)]
    pub(crate) async fn acquire_instance_lifecycle(
        &self,
        instance_id: &str,
    ) -> InstanceLifecycleLease {
        InstanceLifecycleLease::bind(
            instance_id,
            self.instance_lifecycle_gates.clone(),
            self.instance_lifecycle_gates.acquire(instance_id).await,
        )
    }

    pub(crate) async fn try_acquire_instance_lifecycle(
        &self,
        instance_id: &str,
    ) -> Option<InstanceLifecycleLease> {
        Some(InstanceLifecycleLease::bind(
            instance_id,
            self.instance_lifecycle_gates.clone(),
            self.instance_lifecycle_gates
                .try_acquire(instance_id)
                .await?,
        ))
    }

    #[cfg(test)]
    pub(crate) async fn instance_lifecycle_is_held(&self, instance_id: &str) -> bool {
        self.instance_lifecycle_gates.is_held(instance_id).await
    }

    pub(crate) fn mint_known_good_verification_lease(
        &self,
        foreground: &IntegrityForegroundLease,
        lifecycle: &InstanceLifecycleLease,
        expected_library_root: &Path,
    ) -> Result<KnownGoodVerificationLease, KnownGoodVerificationUnavailable> {
        self.validate_integrity_foreground(foreground)
            .map_err(|_| KnownGoodVerificationUnavailable::LiveAuthorityUnavailable)?;
        if !self.instance_lifecycle_gates.owns(&lifecycle.owner) {
            return Err(KnownGoodVerificationUnavailable::LiveAuthorityUnavailable);
        }
        let instance = self
            .instances
            .get(&lifecycle.instance_id)
            .filter(|instance| {
                instance.id == lifecycle.instance_id && is_canonical_instance_id(&instance.id)
            })
            .ok_or(KnownGoodVerificationUnavailable::InstanceNotRegistered)?;
        let library_root = self
            .library_dir()
            .map(PathBuf::from)
            .and_then(|root| known_good::normalize_library_root(&root).ok())
            .ok_or(KnownGoodVerificationUnavailable::LibraryRootUnavailable)?;
        let expected_library_root = known_good::normalize_library_root(expected_library_root)
            .map_err(|_| KnownGoodVerificationUnavailable::LibraryRootUnavailable)?;
        if library_root != expected_library_root {
            return Err(KnownGoodVerificationUnavailable::LiveAuthorityUnavailable);
        }
        let inventory = self
            .known_good
            .active_inventory(
                &instance.id,
                &instance.version_id,
                &instance.created_at,
                &library_root,
            )
            .ok_or(KnownGoodVerificationUnavailable::LiveAuthorityUnavailable)?;

        Ok(KnownGoodVerificationLease {
            owner: KnownGoodVerificationOwner::Foreground(foreground.retained()),
            _lifecycle: lifecycle.retained(),
            instance_id: instance.id,
            version_id: instance.version_id,
            created_at: instance.created_at,
            library_root,
            managed_runtime_cache: self.managed_runtime_cache.clone(),
            inventory,
        })
    }

    pub(crate) fn known_good_verification_lease_can_admit(
        &self,
        lease: &KnownGoodVerificationLease,
    ) -> bool {
        self.known_good_verification_identity_is_current(lease)
            && match &lease.owner {
                KnownGoodVerificationOwner::Foreground(foreground) => {
                    self.validate_integrity_foreground(foreground).is_ok()
                }
                KnownGoodVerificationOwner::IdleSweep(authority) => {
                    self.idle_sweep_authority_is_current(authority)
                }
            }
    }

    pub(crate) fn known_good_verification_lease_is_live(
        &self,
        lease: &KnownGoodVerificationLease,
    ) -> bool {
        self.known_good_verification_identity_is_current(lease)
            && match &lease.owner {
                KnownGoodVerificationOwner::Foreground(foreground) => {
                    self.validate_integrity_foreground(foreground).is_ok()
                }
                KnownGoodVerificationOwner::IdleSweep(authority) => {
                    self.idle_sweep_authority_is_active(authority)
                }
            }
    }

    fn known_good_verification_identity_is_current(
        &self,
        lease: &KnownGoodVerificationLease,
    ) -> bool {
        self.instance_lifecycle_gates.owns(&lease._lifecycle.owner)
            && self.known_good_authority_is_current(
                &lease.instance_id,
                &lease.version_id,
                &lease.created_at,
                &lease.library_root,
                &lease.managed_runtime_cache,
                &lease.inventory,
            )
    }

    fn known_good_authority_is_current(
        &self,
        instance_id: &str,
        version_id: &str,
        created_at: &str,
        expected_library_root: &Path,
        expected_runtime_cache: &ManagedRuntimeCache,
        expected_inventory: &Arc<axial_minecraft::known_good::KnownGoodInventory>,
    ) -> bool {
        let Some(instance) = self.instances.get(instance_id) else {
            return false;
        };
        if instance.id != instance_id
            || instance.version_id != version_id
            || instance.created_at != created_at
        {
            return false;
        }
        let Some(library_root) = self
            .library_dir()
            .map(PathBuf::from)
            .and_then(|root| known_good::normalize_library_root(&root).ok())
        else {
            return false;
        };
        if library_root != expected_library_root {
            return false;
        }
        if self.managed_runtime_cache.root() != expected_runtime_cache.root() {
            return false;
        }
        self.known_good
            .active_inventory(
                &instance.id,
                &instance.version_id,
                &instance.created_at,
                &library_root,
            )
            .is_some_and(|inventory| Arc::ptr_eq(&inventory, expected_inventory))
    }

    #[cfg(test)]
    pub(crate) fn activate_known_good_inventory_for_test(
        &self,
        instance_id: &str,
        inventory: axial_minecraft::known_good::KnownGoodInventory,
    ) {
        drop(self.activate_known_good_inventory_for_test_with_identity(instance_id, inventory));
    }

    #[cfg(test)]
    pub(crate) fn activate_known_good_inventory_for_test_with_identity(
        &self,
        instance_id: &str,
        inventory: axial_minecraft::known_good::KnownGoodInventory,
    ) -> Arc<axial_minecraft::known_good::KnownGoodInventory> {
        let instance = self.instances.get(instance_id).expect("test instance");
        let library_root = self
            .library_dir()
            .map(PathBuf::from)
            .expect("test library root");
        let inventory = Arc::new(inventory);
        self.known_good
            .activate_for_test(
                &instance.id,
                &instance.version_id,
                &instance.created_at,
                &library_root,
                inventory.clone(),
            )
            .expect("activate test known-good inventory");
        inventory
    }

    pub(crate) async fn admit_managed_instance_with_foreground(
        &self,
        foreground: &IntegrityForegroundLease,
        instance_id: &str,
        mutation: bool,
    ) -> Result<AppManagedCompositionAdmission, ManagedInstanceAdmissionError> {
        self.validate_integrity_foreground(foreground)
            .map_err(|_| ManagedInstanceAdmissionError::ForeignForegroundAuthority)?;
        self.admit_managed_instance_inner(instance_id, mutation)
            .await
    }

    #[cfg(test)]
    pub(crate) async fn admit_managed_instance(
        &self,
        instance_id: &str,
        mutation: bool,
    ) -> Result<AppManagedCompositionAdmission, ManagedInstanceAdmissionError> {
        self.admit_managed_instance_inner(instance_id, mutation)
            .await
    }

    async fn admit_managed_instance_inner(
        &self,
        instance_id: &str,
        mutation: bool,
    ) -> Result<AppManagedCompositionAdmission, ManagedInstanceAdmissionError> {
        if !is_canonical_instance_id(instance_id) {
            return Err(ManagedInstanceAdmissionError::InvalidInstanceIdentity);
        }
        if mutation && self.sessions.has_active_instance(instance_id).await {
            return Err(ManagedInstanceAdmissionError::ActiveSession);
        }
        let lifecycle = self.acquire_instance_lifecycle(instance_id).await;
        let instance = self
            .instances
            .get(instance_id)
            .ok_or(ManagedInstanceAdmissionError::InstanceNotFound)?;
        if instance.id != instance_id || !is_canonical_instance_id(&instance.id) {
            return Err(ManagedInstanceAdmissionError::InvalidInstanceIdentity);
        }
        let active_session = self.sessions.has_active_instance(instance_id).await;
        if mutation && active_session {
            return Err(ManagedInstanceAdmissionError::ActiveSession);
        }
        let managed = self
            .performance
            .admit_managed(instance_id, lifecycle, !active_session)
            .await?;
        if self.instances.get(instance_id).as_ref() != Some(&instance) {
            return Err(ManagedInstanceAdmissionError::InstanceNotFound);
        }
        if mutation && self.sessions.has_active_instance(instance_id).await {
            return Err(ManagedInstanceAdmissionError::ActiveSession);
        }
        Ok(managed)
    }

    pub(crate) async fn inspect_managed_instance(
        &self,
        instance_id: &str,
        plan: Option<axial_performance::CompositionPlan>,
    ) -> Result<axial_performance::ManagedCompositionInspection, ManagedInspectionError> {
        let admitted = self
            .admit_managed_instance_inner(instance_id, false)
            .await?;
        let (completed_tx, completed_rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let result = admitted.inspect(plan.as_ref()).await;
            let _ = completed_tx.send(result);
        });
        completed_rx
            .await
            .map_err(|_| ManagedInspectionError::OwnerStopped)?
            .map_err(ManagedInspectionError::Operation)
    }

    pub(crate) async fn resolve_managed_instance(
        &self,
        instance_id: &str,
        request: axial_performance::ResolutionRequest,
    ) -> Result<axial_performance::ManagedResolvedInspection, ManagedInspectionError> {
        let admitted = self
            .admit_managed_instance_inner(instance_id, false)
            .await?;
        let (completed_tx, completed_rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let result = admitted.resolve_and_inspect(request).await;
            let _ = completed_tx.send(result);
        });
        completed_rx
            .await
            .map_err(|_| ManagedInspectionError::OwnerStopped)?
            .map_err(ManagedInspectionError::Operation)
    }

    pub(crate) async fn close_managed_compositions(
        &self,
    ) -> Result<(), ManagedCompositionCloseError> {
        self.performance.close_managed().await
    }

    pub(crate) async fn close_instance_registry(&self) -> Result<(), InstanceStoreError> {
        self.instances.close().await
    }

    pub(crate) async fn close_known_good_inventories(&self) -> std::io::Result<()> {
        self.known_good.close().await
    }

    fn config_commit_observer(&self) -> Arc<dyn Fn(AppConfig, AppConfig) + Send + Sync> {
        let telemetry = self.telemetry.clone();
        let changes = self.config_changes.clone();
        let known_good = self.known_good.clone();
        let installed_versions = self.installed_versions.clone();
        let integrity_activity = self.integrity_activity.clone();
        Arc::new(move |previous: AppConfig, current: AppConfig| {
            if previous.telemetry_enabled && !current.telemetry_enabled {
                telemetry.clear_queue();
            }
            if previous.library_dir != current.library_dir {
                known_good.clear_active();
            }
            if previous.library_dir != current.library_dir
                || previous.library_mode != current.library_mode
            {
                installed_versions.invalidate();
            }
            integrity_activity.invalidate_idle_epoch();
            let _ = changes.send(());
        })
    }

    pub fn flag_enabled(&self, key: &str) -> bool {
        let Some(flag) = find_flag(key) else {
            return false;
        };
        if flag.dev_only && !cfg!(debug_assertions) {
            return false;
        }

        let config = self.config.current();
        let remote_identity = self.remote_flag_identity_for(&config);
        let remote_active = remote_identity.is_some();
        let remote_values = remote_identity
            .as_deref()
            .map(|identity| self.remote_flags.values_snapshot(identity))
            .unwrap_or_default();

        resolve_flag(
            flag,
            &config.feature_overrides,
            remote_active,
            &remote_values,
        )
        .enabled
    }

    pub fn subscribe_config_changes(&self) -> broadcast::Receiver<()> {
        self.config_changes.subscribe()
    }

    pub fn frontend_dir(&self) -> &Path {
        self.frontend_dir.as_path()
    }

    #[cfg(test)]
    pub(crate) fn set_auth_chain_client_override(
        &self,
        client: crate::auth_chain::AuthChainClient,
    ) {
        if let Ok(mut override_slot) = self.auth_chain_client_override.write() {
            *override_slot = Some(client);
        }
    }

    #[cfg(test)]
    pub(crate) fn auth_chain_client_override(&self) -> Option<crate::auth_chain::AuthChainClient> {
        self.auth_chain_client_override
            .read()
            .ok()
            .and_then(|override_slot| override_slot.clone())
    }
}

fn bound_startup_warnings(warnings: Vec<String>) -> Vec<String> {
    warnings
        .into_iter()
        .take(STARTUP_WARNING_LIMIT)
        .map(|warning| warning.chars().take(STARTUP_WARNING_MAX_CHARS).collect())
        .collect()
}

async fn complete_independent_known_good_fanout<C, F, Fut>(
    candidates: Vec<C>,
    mut activate: F,
) -> std::io::Result<()>
where
    F: FnMut(C) -> Fut,
    Fut: std::future::Future<Output = std::io::Result<()>>,
{
    let mut first_error = None;
    for candidate in candidates {
        if let Err(error) = activate(candidate).await
            && first_error.is_none()
        {
            first_error = Some(error);
        }
    }
    first_error.map_or(Ok(()), Err)
}

fn matches_known_good_incarnation(
    instance: Option<&axial_config::Instance>,
    instance_id: &str,
    version_id: &str,
    created_at: &str,
) -> bool {
    instance.is_some_and(|instance| {
        instance.id == instance_id
            && instance.version_id == version_id
            && instance.created_at == created_at
            && is_canonical_instance_id(&instance.id)
    })
}

fn require_matching_known_good_library_root(
    configured_library_root: Option<String>,
    installed_library_root: &Path,
) -> std::io::Result<PathBuf> {
    let configured_library_root = configured_library_root.map(PathBuf::from).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotConnected,
            "known-good library root is not configured",
        )
    })?;
    let configured_library_root = known_good::normalize_library_root(&configured_library_root)?;
    let installed_library_root = known_good::normalize_library_root(installed_library_root)?;
    if configured_library_root != installed_library_root {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "known-good library root changed during installation",
        ));
    }
    Ok(installed_library_root)
}

fn foreign_integrity_foreground_error() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::PermissionDenied,
        "integrity foreground authority belongs to another application state",
    )
}

#[cfg(test)]
mod known_good_identity_tests {
    use super::*;
    use std::sync::Mutex;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn known_good_state_fixture(root: &Path) -> AppState {
        let config_dir = root.join("config");
        let paths = axial_config::AppPaths {
            config_file: config_dir.join("config.json"),
            instances_file: config_dir.join("instances.json"),
            instances_dir: root.join("instances"),
            music_dir: root.join("music"),
            library_dir: root.join("library"),
            config_dir,
        };
        let config = Arc::new(
            axial_config::ConfigStore::load_from(paths.clone()).expect("load test config"),
        );
        let instances = Arc::new(
            axial_config::InstanceStore::from_snapshot(
                paths.clone(),
                axial_config::InstanceRegistrySnapshot::default(),
            )
            .expect("load test instances"),
        );
        AppState::new(AppStateInit {
            app_name: "Axial".to_string(),
            version: "test".to_string(),
            config,
            instances,
            installs: Arc::new(InstallStore::new()),
            sessions: Arc::new(SessionStore::new()),
            performance: Arc::new(
                axial_performance::PerformanceManager::load_for_startup(&paths.config_dir)
                    .expect("load test performance state"),
            ),
            startup_warnings: Vec::new(),
            frontend_dir: root.join("frontend"),
        })
    }

    #[tokio::test]
    async fn integrity_lifecycle_rejects_a_foreign_state_lease() {
        let root = std::env::temp_dir().join(format!(
            "axial-integrity-foreign-state-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        let owner = known_good_state_fixture(&root.join("owner"));
        let foreign = known_good_state_fixture(&root.join("foreign"));
        let foreground = owner
            .register_integrity_foreground()
            .expect("register owner foreground")
            .wait_for_settlement()
            .await;

        assert_eq!(
            foreign
                .acquire_integrity_instance_lifecycle(&foreground, "instance")
                .await
                .err(),
            Some(IntegrityForegroundOwnershipError)
        );
        let foreign_lifecycle = foreign.acquire_instance_lifecycle("instance").await;
        assert_eq!(
            foreign
                .mint_known_good_verification_lease(
                    &foreground,
                    &foreign_lifecycle,
                    Path::new("foreign-library"),
                )
                .err(),
            Some(KnownGoodVerificationUnavailable::LiveAuthorityUnavailable)
        );
        drop(foreign_lifecycle);
        let lifecycle = owner
            .acquire_integrity_instance_lifecycle(&foreground, "instance")
            .await
            .expect("owner accepts its foreground lease");
        drop(lifecycle);
        drop(foreground);
        assert!(owner.subscribe_integrity_idle().borrow().is_stably_idle());

        owner
            .close_known_good_inventories()
            .await
            .expect("close owner known-good store");
        foreign
            .close_known_good_inventories()
            .await
            .expect("close foreign known-good store");
        drop(owner);
        drop(foreign);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn active_integrity_sweep_blocks_installed_version_scan() {
        let root = std::env::temp_dir().join(format!(
            "axial-installed-scan-sweep-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        let state = known_good_state_fixture(&root);
        let library_root = root.join("library");
        std::fs::create_dir_all(axial_minecraft::versions_dir(&library_root))
            .expect("create versions root");
        state.set_library_dir_for_test(library_root.to_string_lossy().into_owned());
        let epoch = state.subscribe_integrity_idle().borrow().epoch();
        let reservation = state
            .try_reserve_idle_sweep(
                epoch,
                state.try_claim_producer().expect("claim sweep producer"),
            )
            .expect("reserve integrity sweep");
        let cancellation = reservation.cancellation();
        let scan = tokio::spawn({
            let state = state.clone();
            async move {
                let producer = state.try_claim_producer().expect("claim scan producer");
                state.installed_versions_snapshot(&producer).await
            }
        });

        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while !cancellation.is_cancelled() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("scan registration cancels active sweep");
        assert!(!scan.is_finished());
        assert_eq!(state.installed_versions_walk_count(), 0);

        drop(reservation);
        let snapshot = tokio::time::timeout(std::time::Duration::from_secs(1), scan)
            .await
            .expect("scan settles after sweep")
            .expect("scan owner");
        assert!(snapshot.is_some());
        assert_eq!(state.installed_versions_walk_count(), 1);
        assert!(state.subscribe_integrity_idle().borrow().is_stably_idle());

        state
            .close_known_good_inventories()
            .await
            .expect("close known-good store");
        drop(state);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn library_root_config_commit_invalidates_installed_version_cache() {
        let root = std::env::temp_dir().join(format!(
            "axial-installed-cache-root-commit-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        let state = known_good_state_fixture(&root);
        let first_root = root.join("first-library");
        let second_root = root.join("second-library");
        std::fs::create_dir_all(axial_minecraft::versions_dir(&first_root))
            .expect("create first versions root");
        std::fs::create_dir_all(axial_minecraft::versions_dir(&second_root))
            .expect("create second versions root");

        let first_root_config = first_root.to_string_lossy().into_owned();
        state
            .mutate_config(move |latest| {
                latest.library_dir = first_root_config;
                latest.library_mode = "existing".to_string();
                Ok(())
            })
            .await
            .expect("publish first library root");
        let producer = state.try_claim_producer().expect("claim lookup producer");
        state
            .installed_versions_snapshot(&producer)
            .await
            .expect("scan first library root");
        state
            .installed_versions_snapshot(&producer)
            .await
            .expect("reuse first library root snapshot");
        assert_eq!(state.installed_versions_walk_count(), 1);

        let second_root_config = second_root.to_string_lossy().into_owned();
        state
            .mutate_config(move |latest| {
                latest.library_dir = second_root_config;
                Ok(())
            })
            .await
            .expect("publish changed library root");
        state
            .installed_versions_snapshot(&producer)
            .await
            .expect("scan changed library root");
        assert_eq!(state.installed_versions_walk_count(), 2);

        drop(producer);
        state
            .close_known_good_inventories()
            .await
            .expect("close known-good store");
        drop(state);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn lifecycle_queued_identity_drift_isolated_from_an_exact_candidate() {
        let root = std::env::temp_dir().join(format!(
            "axial-known-good-identity-drift-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        let state = known_good_state_fixture(&root);
        let library_root = root.join("library");
        std::fs::create_dir_all(&library_root).expect("library root");
        state.set_library_dir_for_test(library_root.to_string_lossy().into_owned());
        let exact = state
            .instances()
            .insert_for_test("Exact", "1.21.5")
            .expect("exact instance");
        let drifted = state
            .instances()
            .insert_for_test("Drifted", "1.21.5")
            .expect("drifted instance");
        let exact_id = exact.id.clone();
        let exact_created_at = exact.created_at.clone();
        let drifted_id = drifted.id.clone();
        let drifted_created_at = drifted.created_at.clone();
        let foreground = state
            .register_integrity_foreground()
            .expect("register fanout foreground")
            .wait_for_settlement()
            .await;
        let fanout_candidates = vec![
            (exact_id.clone(), exact_created_at),
            (drifted_id.clone(), drifted_created_at),
        ];
        let lifecycle = state.acquire_instance_lifecycle(&drifted.id).await;
        let activated = Arc::new(Mutex::new(Vec::new()));
        let first_activated = Arc::new(tokio::sync::Notify::new());
        let fanout_state = state.clone();
        let fanout_root = library_root.clone();
        let fanout_activated = activated.clone();
        let fanout_first_activated = first_activated.clone();
        let fanout_foreground = foreground.retained();
        let fanout = tokio::spawn(async move {
            complete_independent_known_good_fanout(
                fanout_candidates,
                |(instance_id, created_at)| {
                    let state = fanout_state.clone();
                    let library_root = fanout_root.clone();
                    let activated = fanout_activated.clone();
                    let first_activated = fanout_first_activated.clone();
                    let candidate_foreground = fanout_foreground.retained();
                    async move {
                        if let Some(admission) = state
                            .admit_known_good_candidate(
                                &candidate_foreground,
                                &instance_id,
                                "1.21.5",
                                &created_at,
                                &library_root,
                            )
                            .await?
                        {
                            activated
                                .lock()
                                .expect("activated candidates")
                                .push(admission.instance_id.clone());
                            first_activated.notify_one();
                        }
                        Ok(())
                    }
                },
            )
            .await
        });

        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            first_activated.notified(),
        )
        .await
        .expect("first exact candidate should activate before blocked drift candidate");

        let mut replacement = state
            .instances()
            .get(&drifted_id)
            .expect("drifted instance remains registered");
        replacement.version_id = "1.21.6".to_string();
        state
            .instances()
            .replace_for_test(replacement)
            .expect("replace drifted identity");
        drop(lifecycle);

        fanout
            .await
            .expect("fanout task")
            .expect("identity drift is an isolated skip");
        assert_eq!(
            *activated.lock().expect("activated candidates"),
            vec![exact_id],
            "the exact candidate remains activated and the drifted candidate is skipped"
        );
        state
            .close_known_good_inventories()
            .await
            .expect("close known-good store");
        drop(state);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn lifecycle_queued_root_drift_rejects_candidate_admission() {
        let root = std::env::temp_dir().join(format!(
            "axial-known-good-root-drift-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        let state = known_good_state_fixture(&root);
        let installed_root = root.join("library");
        let changed_root = root.join("changed-library");
        std::fs::create_dir_all(&installed_root).expect("installed library root");
        std::fs::create_dir_all(&changed_root).expect("changed library root");
        state.set_library_dir_for_test(installed_root.to_string_lossy().into_owned());
        let instance = state
            .instances()
            .insert_for_test("Root drift", "1.21.5")
            .expect("root-drift instance");
        let foreground = state
            .register_integrity_foreground()
            .expect("register root-drift foreground")
            .wait_for_settlement()
            .await;
        let lifecycle = state.acquire_instance_lifecycle(&instance.id).await;
        let admission_state = state.clone();
        let admission_id = instance.id.clone();
        let admission_created_at = instance.created_at.clone();
        let admission_root = installed_root.clone();
        let admission_foreground = foreground.retained();
        let admission = tokio::spawn(async move {
            admission_state
                .admit_known_good_candidate(
                    &admission_foreground,
                    &admission_id,
                    "1.21.5",
                    &admission_created_at,
                    &admission_root,
                )
                .await
        });

        state.set_library_dir_for_test(changed_root.to_string_lossy().into_owned());
        drop(lifecycle);
        let error = match admission.await.expect("queued root admission") {
            Err(error) => error,
            Ok(_) => panic!("root drift must reject admission"),
        };
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
        state
            .close_known_good_inventories()
            .await
            .expect("close known-good store");
        drop(state);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn admitted_candidate_revalidation_rejects_later_root_drift() {
        let root = std::env::temp_dir().join(format!(
            "axial-known-good-post-admission-root-drift-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        let state = known_good_state_fixture(&root);
        let installed_root = root.join("library");
        let changed_root = root.join("changed-library");
        std::fs::create_dir_all(&installed_root).expect("installed library root");
        std::fs::create_dir_all(&changed_root).expect("changed library root");
        state.set_library_dir_for_test(installed_root.to_string_lossy().into_owned());
        let instance = state
            .instances()
            .insert_for_test("Post-admission root drift", "1.21.5")
            .expect("post-admission instance");
        let foreground = state
            .register_integrity_foreground()
            .expect("register post-admission foreground")
            .wait_for_settlement()
            .await;
        let admission = state
            .admit_known_good_candidate(
                &foreground,
                &instance.id,
                "1.21.5",
                &instance.created_at,
                &installed_root,
            )
            .await
            .expect("admit exact root")
            .expect("exact candidate admission");

        state.set_library_dir_for_test(changed_root.to_string_lossy().into_owned());
        let error = admission
            .revalidate(&state)
            .expect_err("post-admission root drift must fail revalidation");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
        admission.deactivate(&state);
        drop(admission);
        state
            .close_known_good_inventories()
            .await
            .expect("close known-good store");
        drop(state);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn first_failed_activation_does_not_block_a_later_candidate() {
        let activated = Arc::new(Mutex::new(Vec::new()));
        let result = complete_independent_known_good_fanout(
            vec!["first".to_string(), "second".to_string()],
            |candidate| {
                let activated = activated.clone();
                async move {
                    if candidate == "first" {
                        Err(std::io::Error::other("first activation failed"))
                    } else {
                        activated
                            .lock()
                            .expect("activated candidates")
                            .push(candidate);
                        Ok(())
                    }
                }
            },
        )
        .await;

        assert_eq!(
            result.expect_err("first error is retained").to_string(),
            "first activation failed"
        );
        assert_eq!(
            *activated.lock().expect("activated candidates"),
            vec!["second"]
        );
    }

    #[tokio::test]
    async fn later_failed_activation_does_not_undo_an_earlier_candidate() {
        let activated = Arc::new(Mutex::new(Vec::new()));
        let result = complete_independent_known_good_fanout(
            vec!["first".to_string(), "second".to_string()],
            |candidate| {
                let activated = activated.clone();
                async move {
                    if candidate == "second" {
                        Err(std::io::Error::other("second activation failed"))
                    } else {
                        activated
                            .lock()
                            .expect("activated candidates")
                            .push(candidate);
                        Ok(())
                    }
                }
            },
        )
        .await;

        assert_eq!(
            result.expect_err("later error is retained").to_string(),
            "second activation failed"
        );
        assert_eq!(
            *activated.lock().expect("activated candidates"),
            vec!["first"]
        );
    }

    #[test]
    fn unrelated_instance_changes_preserve_known_good_incarnation() {
        let mut instance = new_instance(
            "0000000000000042".to_string(),
            "Before".to_string(),
            "1.21.5".to_string(),
            String::new(),
            String::new(),
        );
        let created_at = instance.created_at.clone();
        assert!(matches_known_good_incarnation(
            Some(&instance),
            &instance.id,
            "1.21.5",
            &created_at,
        ));

        instance.name = "After".to_string();
        instance.max_memory_mb = 8_192;
        instance.icon = "grass".to_string();
        assert!(matches_known_good_incarnation(
            Some(&instance),
            &instance.id,
            "1.21.5",
            &created_at,
        ));

        instance.version_id = "1.21.6".to_string();
        assert!(!matches_known_good_incarnation(
            Some(&instance),
            &instance.id,
            "1.21.5",
            &created_at,
        ));
        assert!(!matches_known_good_incarnation(
            None,
            "0000000000000042",
            "1.21.5",
            &created_at,
        ));
        instance.version_id = "1.21.5".to_string();
        instance.created_at.push_str("-replacement");
        assert!(!matches_known_good_incarnation(
            Some(&instance),
            &instance.id,
            "1.21.5",
            &created_at,
        ));
        instance.created_at = created_at.clone();
        instance.id = "not-canonical".to_string();
        assert!(!matches_known_good_incarnation(
            Some(&instance),
            "not-canonical",
            "1.21.5",
            &created_at,
        ));
    }

    #[test]
    fn receipt_acceptance_requires_the_exact_current_library_root() {
        let root = std::env::temp_dir().join(format!(
            "axial-known-good-root-contract-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        let configured = root.join("configured");
        let changed = root.join("changed");
        std::fs::create_dir_all(&configured).expect("configured root");
        std::fs::create_dir_all(&changed).expect("changed root");

        assert_eq!(
            require_matching_known_good_library_root(None, &configured)
                .expect_err("missing root must fail")
                .kind(),
            std::io::ErrorKind::NotConnected
        );
        assert_eq!(
            require_matching_known_good_library_root(
                Some(configured.to_string_lossy().into_owned()),
                &changed,
            )
            .expect_err("changed root must fail")
            .kind(),
            std::io::ErrorKind::InvalidInput
        );
        let normalized = require_matching_known_good_library_root(
            Some(configured.to_string_lossy().into_owned()),
            &configured,
        )
        .expect("exact root");
        assert_eq!(
            normalized,
            std::fs::canonicalize(&configured).expect("root")
        );

        let _ = std::fs::remove_dir_all(root);
    }
}
