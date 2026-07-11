use super::AppState;
use std::sync::{Arc, Mutex};
use tokio::sync::watch;

const SHUTDOWN_LOCK_INVARIANT: &str =
    "application shutdown lock poisoned; completion state may be inconsistent";
const SHUTDOWN_STEP_COUNT: usize = 18;
type ShutdownAttemptChannel = Arc<watch::Sender<Option<Result<(), AppShutdownError>>>>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AppShutdownStep {
    RequestDrain,
    SessionSettlement,
    DriverSettlement,
    ProducerDrain,
    ManagedCompositions,
    PerformanceRules,
    SkinFlush,
    DriverStore,
    LaunchReports,
    BenchmarkSuites,
    PerformanceOperations,
    Journals,
    FailureMemory,
    Accounts,
    SecureAuth,
    RemoteFlags,
    InstanceRegistry,
    Config,
}

impl AppShutdownStep {
    const fn index(self) -> usize {
        match self {
            Self::RequestDrain => 0,
            Self::SessionSettlement => 1,
            Self::DriverSettlement => 2,
            Self::ProducerDrain => 3,
            Self::ManagedCompositions => 4,
            Self::PerformanceRules => 5,
            Self::SkinFlush => 6,
            Self::DriverStore => 7,
            Self::LaunchReports => 8,
            Self::BenchmarkSuites => 9,
            Self::PerformanceOperations => 10,
            Self::Journals => 11,
            Self::FailureMemory => 12,
            Self::Accounts => 13,
            Self::SecureAuth => 14,
            Self::RemoteFlags => 15,
            Self::InstanceRegistry => 16,
            Self::Config => 17,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::RequestDrain => "request_drain",
            Self::SessionSettlement => "session_settlement",
            Self::DriverSettlement => "driver_settlement",
            Self::ProducerDrain => "producer_drain",
            Self::ManagedCompositions => "managed_compositions",
            Self::PerformanceRules => "performance_rules",
            Self::SkinFlush => "skin_flush",
            Self::DriverStore => "driver_store",
            Self::LaunchReports => "launch_reports",
            Self::BenchmarkSuites => "benchmark_suites",
            Self::PerformanceOperations => "performance_operations",
            Self::Journals => "journals",
            Self::FailureMemory => "failure_memory",
            Self::Accounts => "accounts",
            Self::SecureAuth => "secure_auth",
            Self::RemoteFlags => "remote_flags",
            Self::InstanceRegistry => "instance_registry",
            Self::Config => "config",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("application shutdown is incomplete at {step}", step = .step.as_str())]
pub struct AppShutdownError {
    step: AppShutdownStep,
}

impl AppShutdownError {
    pub const fn step(self) -> AppShutdownStep {
        self.step
    }

    const fn at(step: AppShutdownStep) -> Self {
        Self { step }
    }
}

#[derive(Clone)]
pub(super) struct AppShutdownCoordinator {
    shared: Arc<ShutdownShared>,
}

struct ShutdownShared {
    state: Mutex<ShutdownState>,
}

struct ShutdownState {
    complete: bool,
    completed_steps: [bool; SHUTDOWN_STEP_COUNT],
    in_flight: Option<ShutdownAttemptChannel>,
}

impl AppShutdownCoordinator {
    pub(super) fn new() -> Self {
        Self {
            shared: Arc::new(ShutdownShared {
                state: Mutex::new(ShutdownState {
                    complete: false,
                    completed_steps: [false; SHUTDOWN_STEP_COUNT],
                    in_flight: None,
                }),
            }),
        }
    }

    pub(super) fn start(&self, state: AppState) -> ShutdownAttempt {
        let (attempt, owns_attempt) = {
            let mut shutdown = self.shared.state.lock().expect(SHUTDOWN_LOCK_INVARIANT);
            if shutdown.complete {
                let (_, result) = watch::channel(Some(Ok(())));
                return ShutdownAttempt { result };
            }
            match shutdown.in_flight.as_ref() {
                Some(attempt) => (attempt.clone(), false),
                None => {
                    let (attempt, _) = watch::channel(None);
                    let attempt = Arc::new(attempt);
                    shutdown.in_flight = Some(attempt.clone());
                    (attempt, true)
                }
            }
        };

        if owns_attempt {
            let runner = self.clone();
            let supervisor = self.clone();
            let owned_attempt = attempt.clone();
            tokio::spawn(async move {
                let run = tokio::spawn(async move { runner.coordinate(&state).await });
                let result = run
                    .await
                    .unwrap_or_else(|_| Err(AppShutdownError::at(AppShutdownStep::ProducerDrain)));
                supervisor.finish_attempt(&owned_attempt, result);
            });
        }

        ShutdownAttempt {
            result: attempt.subscribe(),
        }
    }

    async fn coordinate(&self, state: &AppState) -> Result<(), AppShutdownError> {
        if !self.completed(AppShutdownStep::RequestDrain) {
            state
                .lifecycle
                .wait_for_shutdown_started()
                .await
                .map_err(|_| AppShutdownError::at(AppShutdownStep::RequestDrain))?;
            self.mark_completed(AppShutdownStep::RequestDrain);
        }

        let (sessions, drivers) = self.settle_effects(state).await;
        let settlement_error = self.finish_settlement(sessions, drivers);

        let producer_result = if self.completed(AppShutdownStep::ProducerDrain) {
            Ok(())
        } else {
            state
                .lifecycle
                .wait_for_quiesced()
                .await
                .map_err(|_| AppShutdownError::at(AppShutdownStep::ProducerDrain))
        };
        let mut first_error = self.finish_producer_drain(settlement_error, producer_result)?;

        retain_first_error(
            &mut first_error,
            self.close_managed_compositions(state).await,
        );

        let skin_result = self.flush_skin(state).await;
        let (
            benchmark_result,
            performance_result,
            auth_result,
            remote_result,
            rules_result,
            instance_result,
            config_result,
        ) = tokio::join!(
            self.close_benchmark_chain(state),
            self.close_performance_chain(state),
            self.close_auth_chain(state),
            self.close_remote_flags(state),
            self.close_performance_rules(state),
            self.close_instance_registry(state),
            self.close_config(state),
        );

        retain_first_error(&mut first_error, skin_result);
        retain_first_error(&mut first_error, benchmark_result);
        retain_first_error(&mut first_error, performance_result);
        retain_first_error(&mut first_error, auth_result);
        retain_first_error(&mut first_error, remote_result);
        retain_first_error(&mut first_error, rules_result);
        retain_first_error(&mut first_error, instance_result);
        retain_first_error(&mut first_error, config_result);
        first_error.map_or(Ok(()), Err)
    }

    async fn settle_effects(
        &self,
        state: &AppState,
    ) -> (Result<(), AppShutdownError>, Result<(), AppShutdownError>) {
        let sessions = async {
            if self.completed(AppShutdownStep::SessionSettlement) {
                return Ok(());
            }
            state
                .sessions
                .terminate_all()
                .await
                .map_err(|_| AppShutdownError::at(AppShutdownStep::SessionSettlement))
        };
        let drivers = async {
            if self.completed(AppShutdownStep::DriverSettlement) {
                return Ok(());
            }
            state
                .benchmark_suite_drivers
                .stop_all_and_join()
                .await
                .map_err(|_| AppShutdownError::at(AppShutdownStep::DriverSettlement))
        };
        tokio::join!(sessions, drivers)
    }

    fn finish_settlement(
        &self,
        sessions: Result<(), AppShutdownError>,
        drivers: Result<(), AppShutdownError>,
    ) -> Option<AppShutdownError> {
        if sessions.is_ok() {
            self.mark_completed(AppShutdownStep::SessionSettlement);
        }
        if drivers.is_ok() {
            self.mark_completed(AppShutdownStep::DriverSettlement);
        }

        sessions.err().or_else(|| drivers.err())
    }

    fn finish_producer_drain(
        &self,
        settlement_error: Option<AppShutdownError>,
        producer_result: Result<(), AppShutdownError>,
    ) -> Result<Option<AppShutdownError>, AppShutdownError> {
        match producer_result {
            Ok(()) => {
                self.mark_completed(AppShutdownStep::ProducerDrain);
                Ok(settlement_error)
            }
            Err(producer_error) => Err(settlement_error.unwrap_or(producer_error)),
        }
    }

    async fn flush_skin(&self, state: &AppState) -> Result<(), AppShutdownError> {
        if self.completed(AppShutdownStep::SkinFlush) {
            return Ok(());
        }
        crate::application::flush_pending_saved_skin_applies_for_shutdown(state)
            .await
            .map_err(|_| AppShutdownError::at(AppShutdownStep::SkinFlush))?;
        self.mark_completed(AppShutdownStep::SkinFlush);
        Ok(())
    }

    async fn close_benchmark_chain(&self, state: &AppState) -> Result<(), AppShutdownError> {
        let drivers = async {
            if !self.completed(AppShutdownStep::DriverSettlement)
                || self.completed(AppShutdownStep::DriverStore)
            {
                return None;
            }
            Some(
                state
                    .benchmark_suite_drivers
                    .close()
                    .await
                    .map_err(|_| AppShutdownError::at(AppShutdownStep::DriverStore)),
            )
        };
        let reports = async {
            if self.completed(AppShutdownStep::LaunchReports) {
                return None;
            }
            Some(
                state
                    .launch_reports
                    .close()
                    .await
                    .map_err(|_| AppShutdownError::at(AppShutdownStep::LaunchReports)),
            )
        };
        let prerequisites = tokio::join!(drivers, reports);
        self.finish_benchmark_prerequisites(prerequisites.0, prerequisites.1)?;

        if self.benchmark_suite_ready() && !self.completed(AppShutdownStep::BenchmarkSuites) {
            state
                .benchmark_suites
                .close()
                .await
                .map_err(|_| AppShutdownError::at(AppShutdownStep::BenchmarkSuites))?;
            self.mark_completed(AppShutdownStep::BenchmarkSuites);
        }
        Ok(())
    }

    fn finish_benchmark_prerequisites(
        &self,
        drivers: Option<Result<(), AppShutdownError>>,
        reports: Option<Result<(), AppShutdownError>>,
    ) -> Result<(), AppShutdownError> {
        if matches!(drivers, Some(Ok(()))) {
            self.mark_completed(AppShutdownStep::DriverStore);
        }
        if matches!(reports, Some(Ok(()))) {
            self.mark_completed(AppShutdownStep::LaunchReports);
        }

        let mut first_error = None;
        if let Some(result) = drivers {
            retain_first_error(&mut first_error, result);
        }
        if let Some(result) = reports {
            retain_first_error(&mut first_error, result);
        }
        first_error.map_or(Ok(()), Err)
    }

    fn benchmark_suite_ready(&self) -> bool {
        self.completed(AppShutdownStep::DriverSettlement)
            && self.completed(AppShutdownStep::DriverStore)
            && self.completed(AppShutdownStep::LaunchReports)
    }

    async fn close_performance_chain(&self, state: &AppState) -> Result<(), AppShutdownError> {
        let operations = async {
            if self.completed(AppShutdownStep::PerformanceOperations) {
                return Ok(());
            }
            state
                .performance_operations
                .close()
                .await
                .map_err(|_| AppShutdownError::at(AppShutdownStep::PerformanceOperations))
        };
        let journals = async {
            if self.completed(AppShutdownStep::Journals) {
                return Ok(());
            }
            state
                .journals
                .close()
                .await
                .map_err(|_| AppShutdownError::at(AppShutdownStep::Journals))
        };
        let failure_memory = async {
            if self.completed(AppShutdownStep::FailureMemory) {
                return Ok(());
            }
            state
                .failure_memory
                .close()
                .await
                .map_err(|_| AppShutdownError::at(AppShutdownStep::FailureMemory))
        };
        let (operations, journals, failure_memory) =
            tokio::join!(operations, journals, failure_memory);
        self.finish_performance_closes(operations, journals, failure_memory)
    }

    fn finish_performance_closes(
        &self,
        operations: Result<(), AppShutdownError>,
        journals: Result<(), AppShutdownError>,
        failure_memory: Result<(), AppShutdownError>,
    ) -> Result<(), AppShutdownError> {
        if operations.is_ok() {
            self.mark_completed(AppShutdownStep::PerformanceOperations);
        }
        if journals.is_ok() {
            self.mark_completed(AppShutdownStep::Journals);
        }
        if failure_memory.is_ok() {
            self.mark_completed(AppShutdownStep::FailureMemory);
        }

        let mut first_error = None;
        retain_first_error(&mut first_error, operations);
        retain_first_error(&mut first_error, journals);
        retain_first_error(&mut first_error, failure_memory);
        first_error.map_or(Ok(()), Err)
    }

    async fn close_auth_chain(&self, state: &AppState) -> Result<(), AppShutdownError> {
        if !self.completed(AppShutdownStep::SkinFlush) {
            return Ok(());
        }
        if !self.completed(AppShutdownStep::Accounts) {
            state
                .accounts
                .close()
                .await
                .map_err(|_| AppShutdownError::at(AppShutdownStep::Accounts))?;
            self.mark_completed(AppShutdownStep::Accounts);
        }
        if !self.completed(AppShutdownStep::SecureAuth) {
            state
                .auth_logins
                .close()
                .await
                .map_err(|_| AppShutdownError::at(AppShutdownStep::SecureAuth))?;
            self.mark_completed(AppShutdownStep::SecureAuth);
        }
        Ok(())
    }

    async fn close_remote_flags(&self, state: &AppState) -> Result<(), AppShutdownError> {
        if self.completed(AppShutdownStep::RemoteFlags) {
            return Ok(());
        }
        state
            .remote_flags
            .close()
            .await
            .map_err(|_| AppShutdownError::at(AppShutdownStep::RemoteFlags))?;
        self.mark_completed(AppShutdownStep::RemoteFlags);
        Ok(())
    }

    async fn close_performance_rules(&self, state: &AppState) -> Result<(), AppShutdownError> {
        if self.completed(AppShutdownStep::PerformanceRules) {
            return Ok(());
        }
        state
            .close_performance_rules()
            .await
            .map_err(|_| AppShutdownError::at(AppShutdownStep::PerformanceRules))?;
        self.mark_completed(AppShutdownStep::PerformanceRules);
        Ok(())
    }

    async fn close_managed_compositions(&self, state: &AppState) -> Result<(), AppShutdownError> {
        if !self.completed(AppShutdownStep::SessionSettlement) {
            return Err(AppShutdownError::at(AppShutdownStep::SessionSettlement));
        }
        if self.completed(AppShutdownStep::ManagedCompositions) {
            return Ok(());
        }
        state
            .close_managed_compositions()
            .await
            .map_err(|_| AppShutdownError::at(AppShutdownStep::ManagedCompositions))?;
        self.mark_completed(AppShutdownStep::ManagedCompositions);
        Ok(())
    }

    async fn close_config(&self, state: &AppState) -> Result<(), AppShutdownError> {
        if self.completed(AppShutdownStep::Config) {
            return Ok(());
        }
        state
            .close_config()
            .await
            .map_err(|_| AppShutdownError::at(AppShutdownStep::Config))?;
        self.mark_completed(AppShutdownStep::Config);
        Ok(())
    }

    async fn close_instance_registry(&self, state: &AppState) -> Result<(), AppShutdownError> {
        if !self.completed(AppShutdownStep::ManagedCompositions) {
            return Err(AppShutdownError::at(AppShutdownStep::ManagedCompositions));
        }
        if self.completed(AppShutdownStep::InstanceRegistry) {
            return Ok(());
        }
        state
            .close_instance_registry()
            .await
            .map_err(|_| AppShutdownError::at(AppShutdownStep::InstanceRegistry))?;
        self.mark_completed(AppShutdownStep::InstanceRegistry);
        Ok(())
    }

    fn completed(&self, step: AppShutdownStep) -> bool {
        self.shared
            .state
            .lock()
            .expect(SHUTDOWN_LOCK_INVARIANT)
            .completed_steps[step.index()]
    }

    fn mark_completed(&self, step: AppShutdownStep) {
        self.shared
            .state
            .lock()
            .expect(SHUTDOWN_LOCK_INVARIANT)
            .completed_steps[step.index()] = true;
    }

    fn finish_attempt(
        &self,
        attempt: &ShutdownAttemptChannel,
        result: Result<(), AppShutdownError>,
    ) {
        {
            let mut shutdown = self.shared.state.lock().expect(SHUTDOWN_LOCK_INVARIANT);
            if shutdown
                .in_flight
                .as_ref()
                .is_some_and(|current| Arc::ptr_eq(current, attempt))
            {
                shutdown.in_flight = None;
                shutdown.complete = result.is_ok();
            }
        }
        attempt.send_replace(Some(result));
    }
}

fn retain_first_error(
    first_error: &mut Option<AppShutdownError>,
    result: Result<(), AppShutdownError>,
) {
    if first_error.is_none() {
        *first_error = result.err();
    }
}

pub(super) struct ShutdownAttempt {
    result: watch::Receiver<Option<Result<(), AppShutdownError>>>,
}

impl ShutdownAttempt {
    pub(super) async fn wait(mut self) -> Result<(), AppShutdownError> {
        loop {
            if let Some(result) = *self.result.borrow_and_update() {
                return result;
            }
            self.result
                .changed()
                .await
                .map_err(|_| AppShutdownError::at(AppShutdownStep::ProducerDrain))?;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{AppStateInit, InstallStore, SessionStore};
    use axial_config::{AppPaths, ConfigStore, InstanceRegistrySnapshot, InstanceStore};
    use axial_performance::PerformanceManager;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    #[tokio::test]
    async fn cancelled_and_concurrent_callers_share_the_owned_attempt() {
        let fixture = TestFixture::new("owned-attempt");
        let producer = fixture.state.try_claim_producer().expect("claim producer");
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        producer.spawn(async move {
            let _ = entered_tx.send(());
            let _ = release_rx.await;
        });
        entered_rx.await.expect("producer entered");

        let first_state = fixture.state.clone();
        let first = tokio::spawn(async move { first_state.shutdown().await });
        wait_for_phase(
            &fixture.state,
            crate::state::AppLifecyclePhase::QuiescingProducers,
        )
        .await;
        first.abort();
        assert!(first.await.expect_err("cancel first caller").is_cancelled());

        let second_state = fixture.state.clone();
        let second = tokio::spawn(async move { second_state.shutdown().await });
        let third_state = fixture.state.clone();
        let third = tokio::spawn(async move { third_state.shutdown().await });
        assert!(!second.is_finished());
        assert!(!third.is_finished());

        release_tx.send(()).expect("release producer");
        tokio::time::timeout(Duration::from_secs(5), async {
            second
                .await
                .expect("second caller joins")
                .expect("second caller succeeds");
            third
                .await
                .expect("third caller joins")
                .expect("third caller succeeds");
        })
        .await
        .expect("shared shutdown deadline");

        assert_eq!(fixture.state.shutdown().await, Ok(()));
        assert!(fixture.state.try_claim_producer().is_err());
    }

    #[tokio::test]
    async fn producer_drain_failure_reopens_the_attempt_without_closing_stores() {
        let mut fixture = TestFixture::new("producer-drain-retry");
        fixture.state.lifecycle =
            crate::state::AppLifecycle::new_with_deadline(Duration::from_millis(5));
        let producer = fixture.state.try_claim_producer().expect("claim producer");

        assert_eq!(
            fixture.state.shutdown().await,
            Err(AppShutdownError::at(AppShutdownStep::ProducerDrain))
        );
        fixture
            .state
            .accounts()
            .create_offline_account("StillOpen")
            .await
            .expect("producer drain failure leaves stores open");

        drop(producer);
        fixture
            .state
            .shutdown()
            .await
            .expect("later attempt completes after producer settles");
        assert!(
            fixture
                .state
                .accounts()
                .create_offline_account("ClosedStore")
                .await
                .is_err()
        );
        assert!(fixture.state.try_claim_producer().is_err());
    }

    #[test]
    fn shutdown_error_is_copy_bounded_and_step_specific() {
        let error = AppShutdownError::at(AppShutdownStep::LaunchReports);
        let copied = error;
        assert_eq!(copied.step(), AppShutdownStep::LaunchReports);
        assert_eq!(
            copied.to_string(),
            "application shutdown is incomplete at launch_reports"
        );
        assert!(copied.to_string().len() <= 64);
    }

    #[test]
    fn settlement_failure_still_allows_producer_drain_progress() {
        let coordinator = AppShutdownCoordinator::new();
        let settlement_error = coordinator.finish_settlement(
            Err(AppShutdownError::at(AppShutdownStep::SessionSettlement)),
            Ok(()),
        );

        assert_eq!(
            settlement_error,
            Some(AppShutdownError::at(AppShutdownStep::SessionSettlement))
        );
        assert!(!coordinator.completed(AppShutdownStep::SessionSettlement));
        assert!(coordinator.completed(AppShutdownStep::DriverSettlement));

        let first_error = coordinator
            .finish_producer_drain(settlement_error, Ok(()))
            .expect("successful producer drain permits store closes");
        assert_eq!(
            first_error,
            Some(AppShutdownError::at(AppShutdownStep::SessionSettlement))
        );
        assert!(coordinator.completed(AppShutdownStep::ProducerDrain));
    }

    #[tokio::test]
    async fn managed_close_waits_for_session_settlement_before_recovery() {
        const INSTANCE_ID: &str = "0000000000000001";

        let fixture = TestFixture::new("managed-close-session-dependency");
        let coordinator = AppShutdownCoordinator::new();
        let mods_dir = fixture
            .root
            .join("instances")
            .join(INSTANCE_ID)
            .join("mods");
        std::fs::create_dir_all(&mods_dir).expect("create managed instance mods directory");
        let staged = mods_dir.join(".axial-lock.json.new.tmp");
        std::fs::write(&staged, b"not-json").expect("seed ambiguous publication stage");
        let lifecycle = fixture.state.acquire_instance_lifecycle(INSTANCE_ID).await;
        let admitted = fixture
            .state
            .performance
            .admit_managed(INSTANCE_ID, lifecycle, true)
            .await
            .expect("managed admission");
        assert!(matches!(
            admitted.inspect(None).await,
            Err(axial_performance::ManagedMutationError::Indeterminate(_))
        ));
        drop(admitted);
        std::fs::remove_file(staged).expect("repair publication stage");

        let settlement_error = coordinator.finish_settlement(
            Err(AppShutdownError::at(AppShutdownStep::SessionSettlement)),
            Ok(()),
        );
        assert_eq!(
            settlement_error,
            Some(AppShutdownError::at(AppShutdownStep::SessionSettlement))
        );
        assert_eq!(
            coordinator.close_managed_compositions(&fixture.state).await,
            Err(AppShutdownError::at(AppShutdownStep::SessionSettlement))
        );
        assert!(!coordinator.completed(AppShutdownStep::ManagedCompositions));

        let lifecycle = fixture.state.acquire_instance_lifecycle(INSTANCE_ID).await;
        assert!(matches!(
            fixture
                .state
                .performance
                .admit_managed(INSTANCE_ID, lifecycle, false)
                .await,
            Err(super::super::performance_managed::ManagedCompositionAdmissionError::RecoveryBlockedByActiveSession)
        ));

        assert_eq!(coordinator.finish_settlement(Ok(()), Ok(())), None);
        coordinator
            .close_managed_compositions(&fixture.state)
            .await
            .expect("managed close advances after sessions settle");
        assert!(coordinator.completed(AppShutdownStep::ManagedCompositions));
    }

    #[test]
    fn performance_failures_retain_independent_success_for_retry() {
        let coordinator = AppShutdownCoordinator::new();
        let first = coordinator.finish_performance_closes(
            Err(AppShutdownError::at(AppShutdownStep::PerformanceOperations)),
            Ok(()),
            Err(AppShutdownError::at(AppShutdownStep::FailureMemory)),
        );

        assert_eq!(
            first,
            Err(AppShutdownError::at(AppShutdownStep::PerformanceOperations))
        );
        assert!(!coordinator.completed(AppShutdownStep::PerformanceOperations));
        assert!(coordinator.completed(AppShutdownStep::Journals));
        assert!(!coordinator.completed(AppShutdownStep::FailureMemory));

        coordinator
            .finish_performance_closes(Ok(()), Ok(()), Ok(()))
            .expect("retry completes failed independent closes");
        assert!(coordinator.completed(AppShutdownStep::PerformanceOperations));
        assert!(coordinator.completed(AppShutdownStep::Journals));
        assert!(coordinator.completed(AppShutdownStep::FailureMemory));
    }

    #[test]
    fn benchmark_dependencies_skip_then_advance_on_retry() {
        let coordinator = AppShutdownCoordinator::new();
        coordinator
            .finish_benchmark_prerequisites(None, Some(Ok(())))
            .expect("independent launch report close succeeds");

        assert!(!coordinator.completed(AppShutdownStep::DriverSettlement));
        assert!(!coordinator.completed(AppShutdownStep::DriverStore));
        assert!(coordinator.completed(AppShutdownStep::LaunchReports));
        assert!(!coordinator.benchmark_suite_ready());

        coordinator.mark_completed(AppShutdownStep::DriverSettlement);
        let first = coordinator.finish_benchmark_prerequisites(
            Some(Err(AppShutdownError::at(AppShutdownStep::DriverStore))),
            None,
        );

        assert_eq!(
            first,
            Err(AppShutdownError::at(AppShutdownStep::DriverStore))
        );
        assert!(!coordinator.completed(AppShutdownStep::DriverStore));
        assert!(coordinator.completed(AppShutdownStep::LaunchReports));
        assert!(!coordinator.completed(AppShutdownStep::BenchmarkSuites));

        coordinator
            .finish_benchmark_prerequisites(Some(Ok(())), None)
            .expect("retry completes missing prerequisite");
        assert!(coordinator.completed(AppShutdownStep::DriverStore));
        assert!(coordinator.completed(AppShutdownStep::LaunchReports));
        assert!(coordinator.benchmark_suite_ready());
    }

    async fn wait_for_phase(state: &AppState, phase: crate::state::AppLifecyclePhase) {
        tokio::time::timeout(Duration::from_secs(2), async {
            while state.lifecycle_phase() != phase {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("lifecycle phase deadline");
    }

    struct TestFixture {
        state: AppState,
        root: PathBuf,
    }

    impl TestFixture {
        fn new(name: &str) -> Self {
            let root = test_root(name);
            let paths = test_paths(&root);
            let config = Arc::new(ConfigStore::load_from(paths.clone()).expect("load config"));
            let instances = Arc::new(
                InstanceStore::from_snapshot(paths.clone(), InstanceRegistrySnapshot::default())
                    .expect("load instances"),
            );
            let state = AppState::new(AppStateInit {
                app_name: "Axial".to_string(),
                version: "test".to_string(),
                config,
                instances,
                installs: Arc::new(InstallStore::new()),
                sessions: Arc::new(SessionStore::new()),
                performance: Arc::new(
                    PerformanceManager::load_for_startup(&paths.config_dir)
                        .expect("performance manager"),
                ),
                startup_warnings: Vec::new(),
                frontend_dir: root.join("frontend"),
            });
            Self { state, root }
        }
    }

    impl Drop for TestFixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn test_root(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "axial-shutdown-{name}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or_default()
        ));
        std::fs::create_dir_all(&root).expect("create test root");
        root
    }

    fn test_paths(root: &Path) -> AppPaths {
        let config_dir = root.join("config");
        AppPaths {
            config_file: config_dir.join("config.json"),
            instances_file: config_dir.join("instances.json"),
            instances_dir: root.join("instances"),
            music_dir: root.join("music"),
            library_dir: root.join("library"),
            config_dir,
        }
    }
}
