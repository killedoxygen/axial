use super::integrity::{
    PlannedIntegritySweep, ReservedIntegritySweep, Tier2IntegritySweepError,
    plan_tier2_integrity_sweep, reconcile_interrupted_tier2_integrity_sweeps,
};
use crate::guardian::GuardianMode;
use crate::state::{
    AppState, IntegrityIdleEpoch, IntegrityIdleSnapshot, OperationJournalStoreError, ProducerLease,
};
use axial_config::is_canonical_instance_id;
use futures_util::future::BoxFuture;
use std::time::Duration;
use tokio::sync::{broadcast, watch};

#[cfg(test)]
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

const IDLE_INTEGRITY_THRESHOLD: Duration = Duration::from_secs(5 * 60);

trait IntegritySchedulerTransactions: Clone + Send + Sync + 'static {
    type Planned: Send + 'static;
    type Reserved: Send + 'static;
    type ReservationFailure: Send + 'static;

    fn reconcile(
        &self,
        state: AppState,
        producer: ProducerLease,
    ) -> BoxFuture<'static, Result<(), &'static str>>;

    fn plan(
        &self,
        state: AppState,
        producer: ProducerLease,
        instance_id: String,
    ) -> BoxFuture<'static, Result<Self::Planned, SchedulerPlanError>>;

    fn cancel_planned(
        &self,
        planned: Self::Planned,
    ) -> BoxFuture<'static, Result<(), &'static str>>;

    fn reserve(
        &self,
        planned: Self::Planned,
        epoch: IntegrityIdleEpoch,
    ) -> BoxFuture<'static, Result<Self::Reserved, Self::ReservationFailure>>;

    fn reservation_failure_class(&self, failure: &Self::ReservationFailure) -> &'static str;

    fn cancel_reservation_failure(
        &self,
        failure: Self::ReservationFailure,
    ) -> BoxFuture<'static, Result<(), &'static str>>;

    fn reserved_is_current(&self, reserved: &Self::Reserved) -> bool;

    fn cancel_reserved(
        &self,
        reserved: Self::Reserved,
    ) -> BoxFuture<'static, Result<(), &'static str>>;

    fn execute(&self, reserved: Self::Reserved) -> BoxFuture<'static, Result<(), &'static str>>;
}

#[cfg(test)]
#[derive(Clone, Default)]
struct SchedulerTestInstrumentation {
    threshold_arms: Arc<AtomicUsize>,
    accepted_plans: Arc<AtomicUsize>,
    execution_starts: Arc<AtomicUsize>,
}

#[cfg(test)]
impl SchedulerTestInstrumentation {
    fn threshold_armed(&self) {
        self.threshold_arms.fetch_add(1, Ordering::AcqRel);
    }

    async fn wait_for_threshold_arms(&self, expected: usize) {
        for _ in 0..4_096 {
            if self.threshold_arms.load(Ordering::Acquire) >= expected {
                return;
            }
            tokio::task::yield_now().await;
        }
        panic!(
            "expected {expected} armed integrity thresholds, observed {}",
            self.threshold_arms.load(Ordering::Acquire)
        );
    }

    fn threshold_arm_count(&self) -> usize {
        self.threshold_arms.load(Ordering::Acquire)
    }

    fn plan_accepted(&self) {
        self.accepted_plans.fetch_add(1, Ordering::AcqRel);
    }

    fn accepted_plan_count(&self) -> usize {
        self.accepted_plans.load(Ordering::Acquire)
    }

    fn execution_started(&self) {
        self.execution_starts.fetch_add(1, Ordering::AcqRel);
    }

    fn execution_start_count(&self) -> usize {
        self.execution_starts.load(Ordering::Acquire)
    }
}

#[derive(Clone, Copy)]
struct ProductionIntegrityTransactions;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SchedulerPlanError {
    TargetRace,
    DeferredCapacity,
    Failed(&'static str),
}

fn scheduler_journal_plan_error(error: OperationJournalStoreError) -> SchedulerPlanError {
    match error {
        OperationJournalStoreError::CapacityExhausted => SchedulerPlanError::DeferredCapacity,
        error => SchedulerPlanError::Failed(error.class()),
    }
}

impl IntegritySchedulerTransactions for ProductionIntegrityTransactions {
    type Planned = PlannedIntegritySweep;
    type Reserved = ReservedIntegritySweep;
    type ReservationFailure = super::integrity::IntegritySweepReservationFailure;

    fn reconcile(
        &self,
        state: AppState,
        producer: ProducerLease,
    ) -> BoxFuture<'static, Result<(), &'static str>> {
        Box::pin(async move {
            reconcile_interrupted_tier2_integrity_sweeps(&state, producer)
                .await
                .map_err(|error| error.class())
        })
    }

    fn plan(
        &self,
        state: AppState,
        producer: ProducerLease,
        instance_id: String,
    ) -> BoxFuture<'static, Result<Self::Planned, SchedulerPlanError>> {
        Box::pin(async move {
            plan_tier2_integrity_sweep(state, producer, instance_id)
                .await
                .map_err(|error| match error {
                    Tier2IntegritySweepError::InvalidInstanceId
                    | Tier2IntegritySweepError::InstanceNotRegistered => {
                        SchedulerPlanError::TargetRace
                    }
                    Tier2IntegritySweepError::Journal(error) => scheduler_journal_plan_error(error),
                })
        })
    }

    fn cancel_planned(
        &self,
        planned: Self::Planned,
    ) -> BoxFuture<'static, Result<(), &'static str>> {
        Box::pin(async move {
            planned
                .cancel()
                .await
                .map(drop)
                .map_err(|error| error.class())
        })
    }

    fn reserve(
        &self,
        planned: Self::Planned,
        epoch: IntegrityIdleEpoch,
    ) -> BoxFuture<'static, Result<Self::Reserved, Self::ReservationFailure>> {
        Box::pin(async move { planned.reserve(epoch) })
    }

    fn reservation_failure_class(&self, failure: &Self::ReservationFailure) -> &'static str {
        failure.class()
    }

    fn cancel_reservation_failure(
        &self,
        failure: Self::ReservationFailure,
    ) -> BoxFuture<'static, Result<(), &'static str>> {
        Box::pin(async move {
            failure
                .cancel()
                .await
                .map(drop)
                .map_err(|error| error.class())
        })
    }

    fn reserved_is_current(&self, reserved: &Self::Reserved) -> bool {
        reserved.is_current()
    }

    fn cancel_reserved(
        &self,
        reserved: Self::Reserved,
    ) -> BoxFuture<'static, Result<(), &'static str>> {
        Box::pin(async move {
            reserved
                .cancel()
                .await
                .map(drop)
                .map_err(|error| error.class())
        })
    }

    fn execute(&self, reserved: Self::Reserved) -> BoxFuture<'static, Result<(), &'static str>> {
        let execution = reserved.start();
        Box::pin(async move {
            execution
                .wait()
                .await
                .map(drop)
                .map_err(|error| error.class())
        })
    }
}

pub(crate) fn spawn_idle_integrity_scheduler(state: &AppState, producer: ProducerLease) {
    spawn_idle_integrity_scheduler_with_threshold(state, producer, IDLE_INTEGRITY_THRESHOLD);
}

fn spawn_idle_integrity_scheduler_with_threshold(
    state: &AppState,
    producer: ProducerLease,
    threshold: Duration,
) {
    spawn_idle_integrity_scheduler_with_transactions(
        state,
        producer,
        threshold,
        ProductionIntegrityTransactions,
        #[cfg(test)]
        SchedulerTestInstrumentation::default(),
    );
}

fn spawn_idle_integrity_scheduler_with_transactions<Transactions>(
    state: &AppState,
    producer: ProducerLease,
    threshold: Duration,
    transactions: Transactions,
    #[cfg(test)] instrumentation: SchedulerTestInstrumentation,
) where
    Transactions: IntegritySchedulerTransactions,
{
    let supervisor = producer.claim_child();
    let state = state.clone();
    producer.spawn(async move {
        run_idle_integrity_scheduler(
            state,
            supervisor,
            threshold,
            transactions,
            #[cfg(test)]
            instrumentation,
        )
        .await;
    });
}

async fn run_idle_integrity_scheduler<Transactions>(
    state: AppState,
    supervisor: ProducerLease,
    threshold: Duration,
    transactions: Transactions,
    #[cfg(test)] instrumentation: SchedulerTestInstrumentation,
) where
    Transactions: IntegritySchedulerTransactions,
{
    let mut shutdown = state.subscribe_shutdown();
    let Some(reconciliation) = await_transaction_until_shutdown(
        transactions.reconcile(state.clone(), supervisor.claim_child()),
        &mut shutdown,
    )
    .await
    else {
        return;
    };
    if let Err(error_class) = reconciliation {
        tracing::warn!(error_class, "idle integrity startup reconciliation failed");
        return;
    }

    let mut idle = state.subscribe_integrity_idle();
    let mut config_changes = state.subscribe_config_changes();
    let mut cursor = None;

    loop {
        let Some(epoch) = wait_for_stable_idle_epoch(
            &state,
            threshold,
            &mut shutdown,
            &mut idle,
            &mut config_changes,
            #[cfg(test)]
            &instrumentation,
        )
        .await
        else {
            return;
        };
        let Some(instance_id) = next_registered_instance(&state, cursor.as_deref()) else {
            continue;
        };

        let Some(plan) = await_transaction_until_shutdown(
            transactions.plan(state.clone(), supervisor.claim_child(), instance_id.clone()),
            &mut shutdown,
        )
        .await
        else {
            return;
        };
        let planned = match plan {
            Ok(planned) => {
                #[cfg(test)]
                instrumentation.plan_accepted();
                planned
            }
            Err(SchedulerPlanError::TargetRace) => continue,
            Err(SchedulerPlanError::DeferredCapacity) => {
                tracing::warn!(
                    error_class = "capacity_exhausted",
                    "idle integrity plan deferred because operation journal capacity is exhausted"
                );
                continue;
            }
            Err(SchedulerPlanError::Failed(error_class)) => {
                tracing::warn!(error_class, "idle integrity plan persistence failed");
                return;
            }
        };

        if !planned_admission_is_current(
            &state,
            epoch,
            &mut shutdown,
            &mut idle,
            &mut config_changes,
        ) {
            if !terminalize_planned(&transactions, planned, &mut shutdown).await {
                return;
            }
            continue;
        }

        let Some(reservation) =
            await_transaction_until_shutdown(transactions.reserve(planned, epoch), &mut shutdown)
                .await
        else {
            return;
        };
        let reserved = match reservation {
            Ok(reserved) => reserved,
            Err(failure) => {
                let error_class = transactions.reservation_failure_class(&failure);
                let Some(cancellation) = await_transaction_until_shutdown(
                    transactions.cancel_reservation_failure(failure),
                    &mut shutdown,
                )
                .await
                else {
                    return;
                };
                if let Err(cancellation_error_class) = cancellation {
                    tracing::warn!(
                        error_class,
                        cancellation_error_class,
                        "idle integrity reservation cancellation failed"
                    );
                    return;
                }
                continue;
            }
        };

        if !reserved_admission_is_current(
            &state,
            &transactions,
            &reserved,
            &mut shutdown,
            &mut config_changes,
        ) {
            if !terminalize_reserved(&transactions, reserved, &mut shutdown).await {
                return;
            }
            continue;
        }

        #[cfg(test)]
        instrumentation.execution_started();
        let Some(execution) =
            await_transaction_until_shutdown(transactions.execute(reserved), &mut shutdown).await
        else {
            return;
        };
        match execution {
            Ok(_) => cursor = Some(instance_id),
            Err(error_class) => {
                tracing::warn!(error_class, "idle integrity terminal persistence failed");
                return;
            }
        }
    }
}

async fn terminalize_planned<Transactions>(
    transactions: &Transactions,
    planned: Transactions::Planned,
    shutdown: &mut watch::Receiver<bool>,
) -> bool
where
    Transactions: IntegritySchedulerTransactions,
{
    match await_transaction_until_shutdown(transactions.cancel_planned(planned), shutdown).await {
        Some(Ok(())) => true,
        Some(Err(error_class)) => {
            tracing::warn!(error_class, "idle integrity planned cancellation failed");
            false
        }
        None => false,
    }
}

async fn terminalize_reserved<Transactions>(
    transactions: &Transactions,
    reserved: Transactions::Reserved,
    shutdown: &mut watch::Receiver<bool>,
) -> bool
where
    Transactions: IntegritySchedulerTransactions,
{
    match await_transaction_until_shutdown(transactions.cancel_reserved(reserved), shutdown).await {
        Some(Ok(())) => true,
        Some(Err(error_class)) => {
            tracing::warn!(error_class, "idle integrity reserved cancellation failed");
            false
        }
        None => false,
    }
}

async fn await_transaction_until_shutdown<T, E>(
    mut transaction: BoxFuture<'static, Result<T, E>>,
    shutdown: &mut watch::Receiver<bool>,
) -> Option<Result<T, E>> {
    loop {
        if shutdown_requested(shutdown) {
            return None;
        }
        tokio::select! {
            biased;
            changed = shutdown.changed() => {
                if changed.is_err() || shutdown_requested(shutdown) {
                    return None;
                }
            }
            result = &mut transaction => return Some(result),
        }
    }
}

async fn wait_for_stable_idle_epoch(
    state: &AppState,
    threshold: Duration,
    shutdown: &mut watch::Receiver<bool>,
    idle: &mut watch::Receiver<IntegrityIdleSnapshot>,
    config_changes: &mut broadcast::Receiver<()>,
    #[cfg(test)] instrumentation: &SchedulerTestInstrumentation,
) -> Option<IntegrityIdleEpoch> {
    loop {
        if shutdown_requested(shutdown) || drain_config_changes(config_changes).is_closed() {
            return None;
        }
        let snapshot = idle_snapshot(idle);
        if !idle_integrity_enabled(state) || !snapshot.is_stably_idle() {
            if !wait_for_admission_change(shutdown, idle, config_changes).await {
                return None;
            }
            continue;
        }

        let epoch = snapshot.epoch();
        #[cfg(test)]
        instrumentation.threshold_armed();
        let elapsed = tokio::time::sleep(threshold);
        tokio::pin!(elapsed);
        let threshold_elapsed = tokio::select! {
            _ = &mut elapsed => true,
            changed = shutdown.changed() => {
                if changed.is_err() || shutdown_requested(shutdown) {
                    return None;
                }
                false
            }
            changed = idle.changed() => {
                if changed.is_err() {
                    return None;
                }
                false
            }
            event = config_changes.recv() => {
                if matches!(event, Err(broadcast::error::RecvError::Closed)) {
                    return None;
                }
                false
            }
        };
        if !threshold_elapsed {
            continue;
        }

        if shutdown_requested(shutdown)
            || !matches!(drain_config_changes(config_changes), ConfigDrain::Quiet)
            || !idle_integrity_enabled(state)
        {
            continue;
        }
        let current = idle_snapshot(idle);
        if current.is_stably_idle() && current.epoch() == epoch {
            return Some(epoch);
        }
    }
}

async fn wait_for_admission_change(
    shutdown: &mut watch::Receiver<bool>,
    idle: &mut watch::Receiver<IntegrityIdleSnapshot>,
    config_changes: &mut broadcast::Receiver<()>,
) -> bool {
    tokio::select! {
        changed = shutdown.changed() => changed.is_ok() && !shutdown_requested(shutdown),
        changed = idle.changed() => changed.is_ok(),
        event = config_changes.recv() => !matches!(event, Err(broadcast::error::RecvError::Closed)),
    }
}

fn planned_admission_is_current(
    state: &AppState,
    epoch: IntegrityIdleEpoch,
    shutdown: &mut watch::Receiver<bool>,
    idle: &mut watch::Receiver<IntegrityIdleSnapshot>,
    config_changes: &mut broadcast::Receiver<()>,
) -> bool {
    if shutdown_requested(shutdown)
        || !matches!(drain_config_changes(config_changes), ConfigDrain::Quiet)
        || !idle_integrity_enabled(state)
    {
        return false;
    }
    let current = idle_snapshot(idle);
    current.is_stably_idle() && current.epoch() == epoch
}

fn reserved_admission_is_current<Transactions>(
    state: &AppState,
    transactions: &Transactions,
    reserved: &Transactions::Reserved,
    shutdown: &mut watch::Receiver<bool>,
    config_changes: &mut broadcast::Receiver<()>,
) -> bool
where
    Transactions: IntegritySchedulerTransactions,
{
    !shutdown_requested(shutdown)
        && matches!(drain_config_changes(config_changes), ConfigDrain::Quiet)
        && idle_integrity_enabled(state)
        && transactions.reserved_is_current(reserved)
}

fn idle_integrity_enabled(state: &AppState) -> bool {
    let config = state.config().current();
    config.guardian_idle_integrity_enabled
        && GuardianMode::from_config(&config.guardian_mode) == GuardianMode::Managed
}

fn next_registered_instance(state: &AppState, cursor: Option<&str>) -> Option<String> {
    let mut ids = state
        .instances()
        .list()
        .into_iter()
        .map(|instance| instance.id)
        .filter(|instance_id| is_canonical_instance_id(instance_id))
        .collect::<Vec<_>>();
    ids.sort_unstable();
    ids.dedup();
    cursor
        .and_then(|cursor| ids.iter().find(|instance_id| instance_id.as_str() > cursor))
        .cloned()
        .or_else(|| ids.into_iter().next())
}

fn shutdown_requested(shutdown: &mut watch::Receiver<bool>) -> bool {
    *shutdown.borrow_and_update()
}

fn idle_snapshot(idle: &mut watch::Receiver<IntegrityIdleSnapshot>) -> IntegrityIdleSnapshot {
    *idle.borrow_and_update()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ConfigDrain {
    Quiet,
    Changed,
    Closed,
}

impl ConfigDrain {
    const fn is_closed(self) -> bool {
        matches!(self, Self::Closed)
    }
}

fn drain_config_changes(config_changes: &mut broadcast::Receiver<()>) -> ConfigDrain {
    let mut changed = false;
    loop {
        match config_changes.try_recv() {
            Ok(()) | Err(broadcast::error::TryRecvError::Lagged(_)) => changed = true,
            Err(broadcast::error::TryRecvError::Empty) => {
                return if changed {
                    ConfigDrain::Changed
                } else {
                    ConfigDrain::Quiet
                };
            }
            Err(broadcast::error::TryRecvError::Closed) => return ConfigDrain::Closed,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::contracts::{
        CommandKind, JournalId, OperationId, OperationJournalEntry, OperationJournalStep,
        OperationOutcome, OperationPhase, OperationStatus, OperationStepResult, OwnershipClass,
        RollbackState, StabilizationSystem, TargetDescriptor, TargetKind,
    };
    use crate::state::{
        AppStateInit, IdleSweepCancellation, IdleSweepReservation, IdleSweepTerminal, InstallStore,
        OperationJournalStore, SessionStore,
    };
    use axial_config::{AppPaths, ConfigStore, InstanceRegistrySnapshot, InstanceStore};
    use axial_minecraft::known_good::{KnownGoodInventory, TestKnownGoodEntry};
    use axial_performance::PerformanceManager;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{SystemTime, UNIX_EPOCH};
    use std::{fs, io};
    use tokio::sync::Notify;

    const TEST_THRESHOLD: Duration = Duration::from_secs(60);

    #[derive(Default)]
    struct PhaseGate {
        blocked: AtomicBool,
        entered: AtomicBool,
        entered_notify: Notify,
        release: Notify,
    }

    impl PhaseGate {
        fn block(&self) {
            self.blocked.store(true, Ordering::Release);
        }

        async fn pass(&self) {
            self.entered.store(true, Ordering::Release);
            self.entered_notify.notify_waiters();
            loop {
                let released = self.release.notified();
                if !self.blocked.load(Ordering::Acquire) {
                    return;
                }
                released.await;
            }
        }

        async fn wait_until_entered(&self) {
            loop {
                let entered = self.entered_notify.notified();
                if self.entered.load(Ordering::Acquire) {
                    return;
                }
                entered.await;
            }
        }

        fn release(&self) {
            self.blocked.store(false, Ordering::Release);
            self.release.notify_waiters();
        }
    }

    #[derive(Clone)]
    struct ScriptedTransactions {
        inner: Arc<ScriptedTransactionsInner>,
        persist_journals: bool,
        instrumentation: SchedulerTestInstrumentation,
    }

    #[derive(Default)]
    struct ScriptedTransactionsInner {
        next_operation: AtomicUsize,
        worker_starts: AtomicUsize,
        terminal_targets: Mutex<Vec<String>>,
        worker_cancellation: Mutex<Option<IdleSweepCancellation>>,
        reconcile_gate: PhaseGate,
        plan_gate: PhaseGate,
        reserve_gate: PhaseGate,
        cancel_gate: PhaseGate,
        worker_gate: PhaseGate,
        terminal_gate: PhaseGate,
        events: Mutex<Vec<&'static str>>,
    }

    struct ScriptedPlanned {
        state: AppState,
        producer: ProducerLease,
        operation_id: OperationId,
        instance_id: String,
        persist_journal: bool,
    }

    struct ScriptedReserved {
        planned: ScriptedPlanned,
        reservation: IdleSweepReservation,
    }

    struct ScriptedReservationFailure {
        planned: ScriptedPlanned,
        class: &'static str,
    }

    impl Default for ScriptedTransactions {
        fn default() -> Self {
            Self {
                inner: Arc::new(ScriptedTransactionsInner::default()),
                persist_journals: true,
                instrumentation: SchedulerTestInstrumentation::default(),
            }
        }
    }

    impl ScriptedTransactions {
        fn in_memory() -> Self {
            Self {
                inner: Arc::new(ScriptedTransactionsInner::default()),
                persist_journals: false,
                instrumentation: SchedulerTestInstrumentation::default(),
            }
        }

        fn record(&self, event: &'static str) {
            self.inner.events.lock().expect("script events").push(event);
        }

        fn events(&self) -> Vec<&'static str> {
            self.inner.events.lock().expect("script events").clone()
        }

        fn worker_start_count(&self) -> usize {
            self.inner.worker_starts.load(Ordering::Acquire)
        }

        fn terminal_targets(&self) -> Vec<String> {
            self.inner
                .terminal_targets
                .lock()
                .expect("terminal targets")
                .clone()
        }

        async fn wait_for_event(&self, expected: &'static str) {
            self.wait_for_event_count(expected, 1).await;
        }

        async fn wait_for_event_count(&self, expected: &'static str, count: usize) {
            for _ in 0..100_000 {
                if self
                    .events()
                    .into_iter()
                    .filter(|event| *event == expected)
                    .count()
                    >= count
                {
                    return;
                }
                tokio::task::yield_now().await;
            }
            panic!(
                "expected {count} `{expected}` events, observed {:?}",
                self.events()
            );
        }

        async fn wait_for_threshold_arms(&self, count: usize) {
            self.instrumentation.wait_for_threshold_arms(count).await;
        }

        async fn wait_for_worker_cancellation(&self) {
            for _ in 0..4_096 {
                if self
                    .inner
                    .worker_cancellation
                    .lock()
                    .expect("worker cancellation")
                    .as_ref()
                    .is_some_and(IdleSweepCancellation::is_cancelled)
                {
                    return;
                }
                tokio::task::yield_now().await;
            }
            panic!("worker cancellation was not observed");
        }

        fn planned_entry(&self, instance_id: &str) -> OperationJournalEntry {
            let sequence = self.inner.next_operation.fetch_add(1, Ordering::AcqRel) + 1;
            let operation_id = OperationId::new(format!(
                "integrity-sweep-10000000-0000-4000-8000-{sequence:012x}"
            ));
            let mut journal = OperationJournalEntry::new(
                JournalId::new(format!("journal-{}", operation_id.as_str())),
                operation_id,
                CommandKind::ValidateInstance,
                StabilizationSystem::Application,
                OwnershipClass::LauncherManaged,
                RollbackState::NotApplicable,
            );
            journal.targets.push(TargetDescriptor::new(
                StabilizationSystem::Application,
                TargetKind::Instance,
                instance_id,
                OwnershipClass::LauncherManaged,
            ));
            journal.planned_steps.push(OperationJournalStep::new(
                "tier2_integrity_sweep",
                OperationPhase::Validating,
            ));
            journal
        }

        async fn cancel_planned_transaction(planned: ScriptedPlanned) -> Result<(), &'static str> {
            if !planned.persist_journal {
                return Ok(());
            }
            let mut step =
                OperationJournalStep::new("tier2_integrity_sweep", OperationPhase::Validating);
            step.result = OperationStepResult::Skipped;
            planned
                .state
                .journals()
                .record_cancellation(&planned.operation_id, step)
                .await
                .map_err(|error| error.class())
        }
    }

    impl IntegritySchedulerTransactions for ScriptedTransactions {
        type Planned = ScriptedPlanned;
        type Reserved = ScriptedReserved;
        type ReservationFailure = ScriptedReservationFailure;

        fn reconcile(
            &self,
            _state: AppState,
            _producer: ProducerLease,
        ) -> BoxFuture<'static, Result<(), &'static str>> {
            let transactions = self.clone();
            Box::pin(async move {
                transactions.inner.reconcile_gate.pass().await;
                transactions.record("reconciled");
                Ok(())
            })
        }

        fn plan(
            &self,
            state: AppState,
            producer: ProducerLease,
            instance_id: String,
        ) -> BoxFuture<'static, Result<Self::Planned, SchedulerPlanError>> {
            let transactions = self.clone();
            Box::pin(async move {
                transactions.record("plan_started");
                let journal = transactions.planned_entry(&instance_id);
                let operation_id = journal.operation_id.clone();
                if transactions.persist_journals {
                    state
                        .journals()
                        .create(journal)
                        .await
                        .map_err(scheduler_journal_plan_error)?;
                }
                transactions.record("plan_persisted");
                transactions.inner.plan_gate.pass().await;
                Ok(ScriptedPlanned {
                    state,
                    producer,
                    operation_id,
                    instance_id,
                    persist_journal: transactions.persist_journals,
                })
            })
        }

        fn cancel_planned(
            &self,
            planned: Self::Planned,
        ) -> BoxFuture<'static, Result<(), &'static str>> {
            let transactions = self.clone();
            Box::pin(async move {
                transactions.record("planned_cancel_started");
                transactions.inner.cancel_gate.pass().await;
                Self::cancel_planned_transaction(planned).await?;
                transactions.record("planned_cancelled");
                Ok(())
            })
        }

        fn reserve(
            &self,
            planned: Self::Planned,
            epoch: IntegrityIdleEpoch,
        ) -> BoxFuture<'static, Result<Self::Reserved, Self::ReservationFailure>> {
            let transactions = self.clone();
            Box::pin(async move {
                transactions.record("reserve_started");
                transactions.inner.reserve_gate.pass().await;
                match planned
                    .state
                    .try_reserve_idle_sweep(epoch, planned.producer.claim_child())
                {
                    Ok(reservation) => {
                        transactions.record("reserved");
                        Ok(ScriptedReserved {
                            planned,
                            reservation,
                        })
                    }
                    Err(error) => Err(ScriptedReservationFailure {
                        planned,
                        class: match error {
                            crate::state::IdleSweepReserveError::Closing => "closing",
                            crate::state::IdleSweepReserveError::EpochChanged => "epoch_changed",
                            crate::state::IdleSweepReserveError::ForegroundActive => {
                                "foreground_active"
                            }
                            crate::state::IdleSweepReserveError::SweepActive => "sweep_active",
                        },
                    }),
                }
            })
        }

        fn reservation_failure_class(&self, failure: &Self::ReservationFailure) -> &'static str {
            failure.class
        }

        fn cancel_reservation_failure(
            &self,
            failure: Self::ReservationFailure,
        ) -> BoxFuture<'static, Result<(), &'static str>> {
            let transactions = self.clone();
            Box::pin(async move {
                transactions.record("reservation_failure_cancel_started");
                transactions.inner.cancel_gate.pass().await;
                Self::cancel_planned_transaction(failure.planned).await?;
                transactions.record("reservation_failure_cancelled");
                Ok(())
            })
        }

        fn reserved_is_current(&self, reserved: &Self::Reserved) -> bool {
            reserved.reservation.is_current()
        }

        fn cancel_reserved(
            &self,
            reserved: Self::Reserved,
        ) -> BoxFuture<'static, Result<(), &'static str>> {
            let transactions = self.clone();
            Box::pin(async move {
                transactions.record("reserved_cancel_started");
                transactions.inner.cancel_gate.pass().await;
                reserved.reservation.settle(IdleSweepTerminal::Cancelled);
                Self::cancel_planned_transaction(reserved.planned).await?;
                transactions.record("reserved_cancelled");
                Ok(())
            })
        }

        fn execute(
            &self,
            reserved: Self::Reserved,
        ) -> BoxFuture<'static, Result<(), &'static str>> {
            let transactions = self.clone();
            Box::pin(async move {
                let ScriptedReserved {
                    planned,
                    reservation,
                } = reserved;
                transactions
                    .inner
                    .worker_starts
                    .fetch_add(1, Ordering::AcqRel);
                let cancellation = reservation.cancellation();
                *transactions
                    .inner
                    .worker_cancellation
                    .lock()
                    .expect("worker cancellation") = Some(cancellation.clone());
                transactions.record("worker_started");
                let (settled_tx, settled_rx) = tokio::sync::oneshot::channel();
                let worker_transactions = transactions.clone();
                tokio::spawn(async move {
                    worker_transactions.inner.worker_gate.pass().await;
                    let cancelled = cancellation.is_cancelled();
                    reservation.settle(if cancelled {
                        IdleSweepTerminal::Cancelled
                    } else {
                        IdleSweepTerminal::Complete
                    });
                    worker_transactions.record("worker_settled");
                    let _ = settled_tx.send(cancelled);
                });
                let cancelled = settled_rx.await.map_err(|_| "worker_stopped")?;
                transactions.inner.terminal_gate.pass().await;
                if planned.persist_journal {
                    let mut step = OperationJournalStep::new(
                        "tier2_integrity_sweep",
                        OperationPhase::Validating,
                    );
                    if cancelled {
                        step.result = OperationStepResult::Skipped;
                        planned
                            .state
                            .journals()
                            .record_cancellation(&planned.operation_id, step)
                            .await
                            .map_err(|error| error.class())?;
                    } else {
                        step.result = OperationStepResult::Completed;
                        planned
                            .state
                            .journals()
                            .record_success(
                                &planned.operation_id,
                                step,
                                OperationOutcome::Succeeded,
                            )
                            .await
                            .map_err(|error| error.class())?;
                    }
                }
                transactions
                    .inner
                    .terminal_targets
                    .lock()
                    .expect("terminal targets")
                    .push(planned.instance_id);
                transactions.record("terminal_persisted");
                Ok(())
            })
        }
    }

    fn state_fixture(label: &str) -> (AppState, PathBuf) {
        let root = std::env::temp_dir().join(format!(
            "axial-integrity-scheduler-{label}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        let config_dir = root.join("config");
        let paths = AppPaths {
            config_file: config_dir.join("config.json"),
            instances_file: config_dir.join("instances.json"),
            instances_dir: root.join("instances"),
            music_dir: root.join("music"),
            library_dir: root.join("private-library-root"),
            config_dir,
        };
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
        fs::create_dir_all(&paths.library_dir).expect("create library root");
        state.set_library_dir_for_test(paths.library_dir.to_string_lossy().into_owned());
        (state, root)
    }

    fn register_healthy_instance(state: &AppState, name: &str) -> String {
        let instance = state
            .instances()
            .insert_for_test(name, "1.21.5")
            .expect("register instance");
        let inventory = KnownGoodInventory::from_test_entries(Vec::<TestKnownGoodEntry>::new())
            .expect("empty healthy inventory");
        state.activate_known_good_inventory_for_test(&instance.id, inventory);
        instance.id
    }

    fn integrity_journals(state: &AppState) -> Vec<OperationJournalEntry> {
        state
            .journals()
            .list()
            .into_iter()
            .filter(|entry| entry.operation_id.as_str().starts_with("integrity-sweep-"))
            .collect()
    }

    fn terminal_integrity_journals(state: &AppState) -> Vec<OperationJournalEntry> {
        integrity_journals(state)
            .into_iter()
            .filter(|entry| entry.status != OperationStatus::Planned)
            .collect()
    }

    async fn settle_scheduler_start() {
        for _ in 0..16 {
            tokio::task::yield_now().await;
        }
    }

    async fn wait_for_terminal_count(state: &AppState, expected: usize) {
        for _ in 0..4_096 {
            if terminal_integrity_journals(state).len() >= expected {
                return;
            }
            tokio::task::yield_now().await;
        }
        panic!("expected {expected} terminal integrity journals");
    }

    fn spawn_scripted_scheduler_with_threshold(
        state: &AppState,
        transactions: ScriptedTransactions,
        threshold: Duration,
    ) {
        spawn_idle_integrity_scheduler_with_transactions(
            state,
            state.try_claim_producer().expect("claim scheduler"),
            threshold,
            transactions.clone(),
            transactions.instrumentation.clone(),
        );
    }

    fn spawn_scripted_scheduler(state: &AppState, transactions: ScriptedTransactions) {
        spawn_scripted_scheduler_with_threshold(state, transactions, Duration::ZERO);
    }

    async fn start_scripted_scheduler(
        state: &AppState,
        transactions: &ScriptedTransactions,
        threshold: Duration,
    ) {
        spawn_scripted_scheduler_with_threshold(state, transactions.clone(), threshold);
        transactions.wait_for_event("reconciled").await;
        tokio::task::yield_now().await;
    }

    async fn wait_for_shutdown(mut shutdown: watch::Receiver<bool>) {
        loop {
            if *shutdown.borrow_and_update() {
                return;
            }
            shutdown
                .changed()
                .await
                .expect("shutdown sender remains live");
        }
    }

    fn spawn_quiesce(state: &AppState) -> tokio::task::JoinHandle<()> {
        let state = state.clone();
        tokio::spawn(async move {
            state.quiesce().await.expect("scheduler quiesces");
        })
    }

    async fn close_fixture(state: AppState, root: &Path) {
        state.quiesce().await.expect("scheduler quiesces");
        state
            .close_known_good_inventories()
            .await
            .expect("close known-good store");
        state
            .close_instance_registry()
            .await
            .expect("close instance registry");
        drop(state);
        let _ = fs::remove_dir_all(root);
    }

    fn set_scheduler_config(state: &AppState, enabled: bool, mode: &str) {
        let mut config = state.config().current();
        config.guardian_idle_integrity_enabled = enabled;
        config.guardian_mode = mode.to_string();
        state.replace_config_for_test(config);
    }

    fn interrupted_journal() -> OperationJournalEntry {
        let operation_id = OperationId::new("integrity-sweep-00000000-0000-4000-8000-000000000001");
        let mut journal = OperationJournalEntry::new(
            JournalId::new(format!("journal-{}", operation_id.as_str())),
            operation_id,
            CommandKind::ValidateInstance,
            StabilizationSystem::Application,
            OwnershipClass::LauncherManaged,
            RollbackState::NotApplicable,
        );
        journal.targets.push(TargetDescriptor::new(
            StabilizationSystem::Application,
            TargetKind::Instance,
            "0123456789abcdef",
            OwnershipClass::LauncherManaged,
        ));
        journal.planned_steps.push(OperationJournalStep::new(
            "tier2_integrity_sweep",
            OperationPhase::Validating,
        ));
        journal
    }

    #[test]
    fn only_capacity_exhaustion_is_a_deferred_journal_plan_error() {
        assert_eq!(
            scheduler_journal_plan_error(OperationJournalStoreError::CapacityExhausted),
            SchedulerPlanError::DeferredCapacity
        );
        for (error, expected_class) in [
            (OperationJournalStoreError::RetryRequired, "retry_required"),
            (
                OperationJournalStoreError::Persistence(io::Error::other(
                    "injected persistence ambiguity",
                )),
                "persistence",
            ),
        ] {
            assert_eq!(
                scheduler_journal_plan_error(error),
                SchedulerPlanError::Failed(expected_class)
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn journal_capacity_deferral_preserves_cursor_and_requires_a_fresh_threshold() {
        let (state, root) = state_fixture("journal-capacity-deferral");
        let journals = Arc::new(OperationJournalStore::with_max_entries(1));
        let blocker_id = OperationId::new("active-capacity-blocker");
        journals
            .create(OperationJournalEntry::new(
                JournalId::new("journal-active-capacity-blocker"),
                blocker_id.clone(),
                CommandKind::InstallVersion,
                StabilizationSystem::Application,
                OwnershipClass::LauncherManaged,
                RollbackState::NotApplicable,
            ))
            .await
            .expect("fill journal capacity with an active operation");
        let performance_operations = state.performance_operations().clone();
        let state = state.with_operation_stores(journals.clone(), performance_operations);
        let mut instance_ids = [
            register_healthy_instance(&state, "Second after capacity"),
            register_healthy_instance(&state, "First after capacity"),
        ];
        instance_ids.sort();
        let instrumentation = SchedulerTestInstrumentation::default();
        spawn_idle_integrity_scheduler_with_transactions(
            &state,
            state.try_claim_producer().expect("claim scheduler"),
            TEST_THRESHOLD,
            ProductionIntegrityTransactions,
            instrumentation.clone(),
        );
        instrumentation.wait_for_threshold_arms(1).await;

        tokio::time::advance(TEST_THRESHOLD).await;
        instrumentation.wait_for_threshold_arms(2).await;
        assert_eq!(instrumentation.threshold_arm_count(), 2);
        assert!(integrity_journals(&state).is_empty());
        assert!(state.subscribe_integrity_idle().borrow().is_stably_idle());
        assert_eq!(instrumentation.accepted_plan_count(), 0);
        assert_eq!(instrumentation.execution_start_count(), 0);
        for _ in 0..256 {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            instrumentation.threshold_arm_count(),
            2,
            "capacity rejection must not busy-retry planning"
        );
        assert!(integrity_journals(&state).is_empty());
        assert_eq!(instrumentation.accepted_plan_count(), 0);
        assert_eq!(instrumentation.execution_start_count(), 0);

        let mut completed = OperationJournalStep::new("capacity_released", OperationPhase::Running);
        completed.result = OperationStepResult::Completed;
        journals
            .record_success(&blocker_id, completed, OperationOutcome::Succeeded)
            .await
            .expect("terminalize capacity blocker");
        tokio::time::advance(TEST_THRESHOLD - Duration::from_secs(1)).await;
        settle_scheduler_start().await;
        assert!(integrity_journals(&state).is_empty());
        assert_eq!(instrumentation.threshold_arm_count(), 2);
        assert_eq!(instrumentation.accepted_plan_count(), 0);
        assert_eq!(instrumentation.execution_start_count(), 0);

        tokio::time::advance(Duration::from_secs(1)).await;
        wait_for_terminal_count(&state, 1).await;
        let terminals = terminal_integrity_journals(&state);
        assert_eq!(terminals.len(), 1);
        assert_eq!(terminals[0].targets[0].id, instance_ids[0]);
        assert_eq!(instrumentation.accepted_plan_count(), 1);
        assert_eq!(instrumentation.execution_start_count(), 1);
        close_fixture(state, &root).await;
    }

    #[tokio::test]
    async fn production_execute_survives_an_unpolled_waiter_dropped_for_shutdown() {
        let (state, root) = state_fixture("unpolled-production-execute");
        let instance_id = register_healthy_instance(&state, "Unpolled production execute");
        let planned = plan_tier2_integrity_sweep(
            state.clone(),
            state
                .try_claim_producer()
                .expect("claim production sweep producer"),
            instance_id,
        )
        .await
        .expect("plan production sweep");
        let epoch = state.subscribe_integrity_idle().borrow().epoch();
        let reserved = match planned.reserve(epoch) {
            Ok(reserved) => reserved,
            Err(failure) => panic!("reserve production sweep: {}", failure.class()),
        };

        let unpolled_waiter = ProductionIntegrityTransactions.execute(reserved);
        let quiesce = spawn_quiesce(&state);
        drop(unpolled_waiter);

        tokio::time::timeout(Duration::from_secs(5), quiesce)
            .await
            .expect("detached production owner reaches its durable terminal")
            .expect("quiesce waiter");
        let terminals = terminal_integrity_journals(&state);
        assert_eq!(terminals.len(), 1);
        assert!(matches!(
            terminals[0].status,
            OperationStatus::Succeeded | OperationStatus::Cancelled
        ));
        close_fixture(state, &root).await;
    }

    #[tokio::test(start_paused = true)]
    async fn exact_epoch_aba_requires_a_fresh_full_threshold() {
        let (state, root) = state_fixture("epoch-aba");
        let instance_id = register_healthy_instance(&state, "ABA");
        let transactions = ScriptedTransactions::in_memory();
        start_scripted_scheduler(&state, &transactions, TEST_THRESHOLD).await;
        transactions.wait_for_threshold_arms(1).await;

        tokio::time::advance(TEST_THRESHOLD / 2).await;
        let foreground = state
            .register_integrity_foreground()
            .expect("register foreground")
            .wait_for_settlement()
            .await;
        drop(foreground);
        transactions.wait_for_threshold_arms(2).await;
        tokio::time::advance(TEST_THRESHOLD / 2).await;
        settle_scheduler_start().await;
        assert!(transactions.terminal_targets().is_empty());

        tokio::time::advance(TEST_THRESHOLD / 2).await;
        transactions.wait_for_event("terminal_persisted").await;
        assert_eq!(transactions.terminal_targets(), [instance_id]);
        close_fixture(state, &root).await;
    }

    #[tokio::test(start_paused = true)]
    async fn disabled_custom_and_any_config_commit_reset_admission() {
        let (state, root) = state_fixture("config-reset");
        let instance_id = register_healthy_instance(&state, "Config");
        set_scheduler_config(&state, false, "managed");
        let transactions = ScriptedTransactions::in_memory();
        start_scripted_scheduler(&state, &transactions, TEST_THRESHOLD).await;

        tokio::time::advance(TEST_THRESHOLD * 2).await;
        settle_scheduler_start().await;
        assert!(transactions.terminal_targets().is_empty());
        set_scheduler_config(&state, true, "custom");
        tokio::time::advance(TEST_THRESHOLD * 2).await;
        settle_scheduler_start().await;
        assert!(transactions.terminal_targets().is_empty());

        set_scheduler_config(&state, true, "managed");
        transactions.wait_for_threshold_arms(1).await;
        tokio::time::advance(TEST_THRESHOLD / 2).await;
        let epoch_before_commit = state.subscribe_integrity_idle().borrow().epoch();
        let mut config = state.config().current();
        config.music_track = config.music_track.saturating_add(1);
        state.replace_config_for_test(config);
        assert_ne!(
            state.subscribe_integrity_idle().borrow().epoch(),
            epoch_before_commit
        );
        transactions.wait_for_threshold_arms(2).await;
        tokio::time::advance(TEST_THRESHOLD / 2).await;
        settle_scheduler_start().await;
        assert!(transactions.terminal_targets().is_empty());
        tokio::time::advance(TEST_THRESHOLD / 2).await;
        transactions.wait_for_event("terminal_persisted").await;
        assert_eq!(transactions.terminal_targets(), [instance_id]);
        close_fixture(state, &root).await;
    }

    #[tokio::test(start_paused = true)]
    async fn lexical_cursor_runs_one_instance_per_fresh_threshold() {
        let (state, root) = state_fixture("lexical-cursor");
        let mut expected = [
            register_healthy_instance(&state, "Third"),
            register_healthy_instance(&state, "First"),
            register_healthy_instance(&state, "Second"),
        ];
        expected.sort();
        let transactions = ScriptedTransactions::in_memory();
        start_scripted_scheduler(&state, &transactions, TEST_THRESHOLD).await;
        transactions.wait_for_threshold_arms(1).await;

        for (index, expected_id) in expected.iter().enumerate() {
            tokio::time::advance(TEST_THRESHOLD - Duration::from_secs(1)).await;
            settle_scheduler_start().await;
            assert_eq!(transactions.terminal_targets().len(), index);
            tokio::time::advance(Duration::from_secs(1)).await;
            transactions
                .wait_for_event_count("terminal_persisted", index + 1)
                .await;
            assert_eq!(transactions.terminal_targets().last(), Some(expected_id));
            if index + 1 < expected.len() {
                transactions.wait_for_threshold_arms(index + 2).await;
            }
        }
        close_fixture(state, &root).await;
    }

    #[tokio::test(start_paused = true)]
    async fn empty_registry_consumes_a_threshold_without_carrying_admission() {
        let (state, root) = state_fixture("empty-registry");
        let transactions = ScriptedTransactions::in_memory();
        start_scripted_scheduler(&state, &transactions, TEST_THRESHOLD).await;
        transactions.wait_for_threshold_arms(1).await;
        tokio::time::advance(TEST_THRESHOLD).await;
        transactions.wait_for_threshold_arms(2).await;
        assert!(transactions.terminal_targets().is_empty());

        let instance_id = register_healthy_instance(&state, "Later");
        tokio::time::advance(TEST_THRESHOLD - Duration::from_secs(1)).await;
        settle_scheduler_start().await;
        assert!(transactions.terminal_targets().is_empty());
        tokio::time::advance(Duration::from_secs(1)).await;
        transactions.wait_for_event("terminal_persisted").await;
        assert_eq!(transactions.terminal_targets(), [instance_id]);
        close_fixture(state, &root).await;
    }

    #[tokio::test(start_paused = true)]
    async fn startup_reconciliation_precedes_disabled_scheduler_admission() {
        let (state, root) = state_fixture("startup-reconciliation");
        state
            .journals()
            .create(interrupted_journal())
            .await
            .expect("persist interrupted plan");
        set_scheduler_config(&state, false, "managed");
        spawn_idle_integrity_scheduler_with_threshold(
            &state,
            state.try_claim_producer().expect("claim scheduler"),
            TEST_THRESHOLD,
        );

        wait_for_terminal_count(&state, 1).await;
        assert_eq!(
            integrity_journals(&state)[0].status,
            OperationStatus::Cancelled
        );
        tokio::time::advance(TEST_THRESHOLD * 2).await;
        settle_scheduler_start().await;
        assert_eq!(integrity_journals(&state).len(), 1);
        close_fixture(state, &root).await;
    }

    #[tokio::test]
    async fn shutdown_drops_blocked_plan_owner_and_leaves_durable_plan_for_restart() {
        let (state, root) = state_fixture("post-plan-shutdown");
        register_healthy_instance(&state, "Post plan shutdown");
        let transactions = ScriptedTransactions::default();
        transactions.inner.plan_gate.block();
        spawn_scripted_scheduler(&state, transactions.clone());

        transactions.inner.plan_gate.wait_until_entered().await;
        assert_eq!(
            integrity_journals(&state)[0].status,
            OperationStatus::Planned
        );
        assert_eq!(transactions.worker_start_count(), 0);

        let shutdown = state.subscribe_shutdown();
        let quiesce = spawn_quiesce(&state);
        wait_for_shutdown(shutdown).await;
        tokio::time::timeout(Duration::from_secs(1), quiesce)
            .await
            .expect("blocked plan does not prevent quiescence")
            .expect("join quiesce");
        assert_eq!(
            integrity_journals(&state)[0].status,
            OperationStatus::Planned
        );
        assert_eq!(transactions.worker_start_count(), 0);
        assert_eq!(
            transactions.events(),
            ["reconciled", "plan_started", "plan_persisted"]
        );
        transactions.inner.plan_gate.release();
        close_fixture(state, &root).await;
    }

    #[tokio::test]
    async fn durable_plan_is_cancelled_after_post_plan_config_change_without_starting_a_worker() {
        let (state, root) = state_fixture("post-plan-config");
        register_healthy_instance(&state, "Post plan config");
        let transactions = ScriptedTransactions::default();
        transactions.inner.plan_gate.block();
        spawn_scripted_scheduler(&state, transactions.clone());

        transactions.inner.plan_gate.wait_until_entered().await;
        assert_eq!(
            integrity_journals(&state)[0].status,
            OperationStatus::Planned
        );
        set_scheduler_config(&state, false, "managed");
        transactions.inner.plan_gate.release();

        wait_for_terminal_count(&state, 1).await;
        transactions.wait_for_event("planned_cancelled").await;
        assert_eq!(
            terminal_integrity_journals(&state)[0].status,
            OperationStatus::Cancelled
        );
        assert_eq!(transactions.worker_start_count(), 0);
        assert_eq!(
            transactions.events(),
            [
                "reconciled",
                "plan_started",
                "plan_persisted",
                "planned_cancel_started",
                "planned_cancelled",
            ]
        );
        close_fixture(state, &root).await;
    }

    #[tokio::test]
    async fn shutdown_drops_blocked_cancellation_and_leaves_durable_plan_for_restart() {
        let (state, root) = state_fixture("blocked-cancellation-shutdown");
        register_healthy_instance(&state, "Blocked cancellation shutdown");
        let transactions = ScriptedTransactions::default();
        transactions.inner.plan_gate.block();
        transactions.inner.cancel_gate.block();
        spawn_scripted_scheduler(&state, transactions.clone());

        transactions.inner.plan_gate.wait_until_entered().await;
        set_scheduler_config(&state, false, "managed");
        transactions.inner.plan_gate.release();
        transactions.inner.cancel_gate.wait_until_entered().await;
        assert_eq!(
            integrity_journals(&state)[0].status,
            OperationStatus::Planned
        );

        let shutdown = state.subscribe_shutdown();
        let quiesce = spawn_quiesce(&state);
        wait_for_shutdown(shutdown).await;
        tokio::time::timeout(Duration::from_secs(1), quiesce)
            .await
            .expect("blocked cancellation does not prevent quiescence")
            .expect("join quiesce");
        assert_eq!(
            integrity_journals(&state)[0].status,
            OperationStatus::Planned
        );
        assert_eq!(
            transactions.events(),
            [
                "reconciled",
                "plan_started",
                "plan_persisted",
                "planned_cancel_started",
            ]
        );
        transactions.inner.cancel_gate.release();
        close_fixture(state, &root).await;
    }

    #[tokio::test]
    async fn foreground_race_during_reservation_durably_cancels_the_plan() {
        let (state, root) = state_fixture("reserve-foreground-race");
        register_healthy_instance(&state, "Reserve foreground race");
        let transactions = ScriptedTransactions::default();
        transactions.inner.reserve_gate.block();
        spawn_scripted_scheduler(&state, transactions.clone());

        transactions.inner.reserve_gate.wait_until_entered().await;
        assert_eq!(
            integrity_journals(&state)[0].status,
            OperationStatus::Planned
        );
        let foreground = state
            .register_integrity_foreground()
            .expect("register foreground")
            .wait_for_settlement()
            .await;
        transactions.inner.reserve_gate.release();

        wait_for_terminal_count(&state, 1).await;
        transactions
            .wait_for_event("reservation_failure_cancelled")
            .await;
        assert_eq!(
            terminal_integrity_journals(&state)[0].status,
            OperationStatus::Cancelled
        );
        assert_eq!(transactions.worker_start_count(), 0);
        assert_eq!(
            transactions.events(),
            [
                "reconciled",
                "plan_started",
                "plan_persisted",
                "reserve_started",
                "reservation_failure_cancel_started",
                "reservation_failure_cancelled",
            ]
        );
        set_scheduler_config(&state, false, "managed");
        drop(foreground);
        close_fixture(state, &root).await;
    }

    #[tokio::test]
    async fn foreground_waits_for_worker_settlement_but_not_terminal_journal_io() {
        let (state, root) = state_fixture("foreground-worker-ownership");
        register_healthy_instance(&state, "Foreground worker ownership");
        let transactions = ScriptedTransactions::default();
        transactions.inner.worker_gate.block();
        transactions.inner.terminal_gate.block();
        spawn_scripted_scheduler(&state, transactions.clone());

        transactions.inner.worker_gate.wait_until_entered().await;
        assert_eq!(transactions.worker_start_count(), 1);
        assert_eq!(
            integrity_journals(&state)[0].status,
            OperationStatus::Planned
        );
        let foreground = state
            .register_integrity_foreground()
            .expect("register foreground");
        let foreground = tokio::spawn(foreground.wait_for_settlement());
        assert!(!foreground.is_finished());
        transactions.wait_for_worker_cancellation().await;

        transactions.inner.worker_gate.release();
        transactions.inner.terminal_gate.wait_until_entered().await;
        let foreground = foreground.await.expect("join foreground waiter");
        assert_eq!(
            integrity_journals(&state)[0].status,
            OperationStatus::Planned
        );
        assert!(!transactions.events().contains(&"terminal_persisted"));

        set_scheduler_config(&state, false, "managed");
        transactions.inner.terminal_gate.release();
        transactions.wait_for_event("terminal_persisted").await;
        assert_eq!(
            terminal_integrity_journals(&state)[0].status,
            OperationStatus::Cancelled
        );
        drop(foreground);
        close_fixture(state, &root).await;
    }

    #[tokio::test]
    async fn shutdown_drops_execute_waiter_but_worker_retains_lifecycle_until_settlement() {
        let (state, root) = state_fixture("shutdown-worker-ownership");
        register_healthy_instance(&state, "Shutdown worker ownership");
        let transactions = ScriptedTransactions::default();
        transactions.inner.worker_gate.block();
        transactions.inner.terminal_gate.block();
        spawn_scripted_scheduler(&state, transactions.clone());

        transactions.inner.worker_gate.wait_until_entered().await;
        let shutdown = state.subscribe_shutdown();
        let quiesce = spawn_quiesce(&state);
        wait_for_shutdown(shutdown).await;
        assert!(!quiesce.is_finished());
        transactions.wait_for_worker_cancellation().await;

        transactions.inner.worker_gate.release();
        tokio::time::timeout(Duration::from_secs(1), quiesce)
            .await
            .expect("sweep settlement releases the final lifecycle owner")
            .expect("join quiesce");
        assert_eq!(
            integrity_journals(&state)[0].status,
            OperationStatus::Planned
        );
        assert_eq!(
            transactions.events(),
            [
                "reconciled",
                "plan_started",
                "plan_persisted",
                "reserve_started",
                "reserved",
                "worker_started",
                "worker_settled",
            ]
        );
        transactions.inner.terminal_gate.release();
        close_fixture(state, &root).await;
    }

    #[tokio::test]
    async fn shutdown_drops_blocked_startup_reconciliation_owner() {
        let (state, root) = state_fixture("blocked-startup-reconciliation");
        let transactions = ScriptedTransactions::default();
        transactions.inner.reconcile_gate.block();
        spawn_scripted_scheduler(&state, transactions.clone());

        transactions.inner.reconcile_gate.wait_until_entered().await;
        let shutdown = state.subscribe_shutdown();
        let quiesce = spawn_quiesce(&state);
        wait_for_shutdown(shutdown).await;
        tokio::time::timeout(Duration::from_secs(1), quiesce)
            .await
            .expect("blocked reconciliation does not prevent quiescence")
            .expect("join quiesce");
        assert!(transactions.events().is_empty());
        transactions.inner.reconcile_gate.release();
        close_fixture(state, &root).await;
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_before_threshold_stops_without_a_plan() {
        let (state, root) = state_fixture("shutdown-before-threshold");
        register_healthy_instance(&state, "Shutdown");
        spawn_idle_integrity_scheduler_with_threshold(
            &state,
            state.try_claim_producer().expect("claim scheduler"),
            TEST_THRESHOLD,
        );
        settle_scheduler_start().await;
        tokio::time::advance(TEST_THRESHOLD / 2).await;

        state.quiesce().await.expect("scheduler exits on shutdown");
        tokio::time::advance(TEST_THRESHOLD).await;
        settle_scheduler_start().await;
        assert!(integrity_journals(&state).is_empty());
        close_fixture(state, &root).await;
    }

    #[test]
    fn shared_startup_aggregator_orders_repair_before_every_supervisor() {
        let app = include_str!("../app.rs");
        let repair = app
            .find("settle_startup_persisted_state_repairs(state).await")
            .expect("awaited persisted-state repair barrier");
        for call in [
            "spawn_known_good_rebuilds(state);",
            "spawn_idle_integrity_scheduler(state);",
            "spawn_performance_operations_resume(state);",
            "spawn_benchmark_suite_drivers_resume(state);",
            "spawn_performance_rules_refresh(state);",
            "spawn_telemetry_export(state);",
            "spawn_remote_flags_refresh(state);",
        ] {
            assert!(repair < app.find(call).expect("aggregated startup call"));
        }
        for source in [
            include_str!("../main.rs"),
            include_str!("../../../desktop/src/main.rs"),
        ] {
            assert!(source.contains("start_application_background_workflows(&state).await"));
            assert!(!source.contains("spawn_performance_operations_resume"));
            assert!(!source.contains("spawn_benchmark_suite_drivers_resume"));
        }
    }

    #[test]
    fn lagged_config_broadcast_is_an_admission_reset() {
        let (changes, _) = broadcast::channel(2);
        let mut receiver = changes.subscribe();
        for _ in 0..4 {
            let _ = changes.send(());
        }

        assert_eq!(drain_config_changes(&mut receiver), ConfigDrain::Changed);
    }

    #[test]
    fn scheduler_source_has_a_fixed_threshold_and_no_polling_or_manual_surface() {
        let source = include_str!("integrity_scheduler.rs");
        let production = source
            .split("\n#[cfg(test)]\nmod tests {")
            .next()
            .expect("production scheduler source");

        assert!(production.contains("Duration::from_secs(5 * 60)"));
        assert!(!production.contains("std::env"));
        assert!(!production.contains("interval("));
        assert!(!production.contains("record_progress"));
        assert!(!production.contains("failure_memory"));
        assert!(!production.contains("repair"));
        assert!(!production.contains("decide_guardian_policy"));
    }
}
