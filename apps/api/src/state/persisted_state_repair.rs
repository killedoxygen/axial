use super::AppState;
use super::failure_memory::{
    FailureMemoryKey, FailureMemoryStoreError, GuardianFailureMemoryEntry,
    PersistedStateRepairReservation, PersistedStateRepairReserveError,
};
use super::journals::{
    OperationJournalReconciliation, OperationJournalStoreError,
    persisted_state_repair_plan_is_visible, persisted_state_repair_terminal_is_visible,
};
use super::persisted_state_load::{
    PersistedStateRejectedRecordEligibility, PersistedStateRejectedRecordQuarantineReceipt,
};
use crate::execution::anchored_record::{
    AnchoredRecordDirectory, AnchoredRecordQuarantinePreservationError,
};
use crate::guardian::persisted_state_repair::{
    PERSISTED_STATE_REPAIR_CANDIDATES, PersistedStateRepairAssessmentProof,
};
use crate::guardian::{DiagnosisId, GuardianActionKind, GuardianDecision, GuardianMode};
use crate::state::contracts::{OwnershipClass, StabilizationSystem};
use crate::state::contracts::{
    PersistedStateRepairAttempt, PersistedStateRepairTerminal, PersistedStateRepairTerminalOutcome,
    persisted_state_repair_quarantine_suffix,
};
use chrono::{DateTime, Utc};
use std::collections::BTreeMap;
use std::io;
use std::time::Duration;
use tokio::sync::OwnedMutexGuard;

const PERSISTED_STATE_REPAIR_JOURNAL_RETRY_INITIAL: Duration = Duration::from_millis(20);
const PERSISTED_STATE_REPAIR_JOURNAL_RETRY_MAX: Duration = Duration::from_secs(1);
const PERSISTED_STATE_REPAIR_MEMORY_SETTLEMENT_ATTEMPTS: usize = 4;

pub(super) struct PersistedStateRepairDirectories {
    performance_operations: AnchoredRecordDirectory,
    benchmark_suite_drivers: AnchoredRecordDirectory,
}

impl PersistedStateRepairDirectories {
    pub(super) fn new(
        performance_operations: AnchoredRecordDirectory,
        benchmark_suite_drivers: AnchoredRecordDirectory,
    ) -> Self {
        Self {
            performance_operations,
            benchmark_suite_drivers,
        }
    }

    fn for_store(
        &self,
        store: super::contracts::PersistedStateRecordStore,
    ) -> AnchoredRecordDirectory {
        match store {
            super::contracts::PersistedStateRecordStore::PerformanceOperation => {
                self.performance_operations.clone()
            }
            super::contracts::PersistedStateRecordStore::BenchmarkSuiteDriver => {
                self.benchmark_suite_drivers.clone()
            }
        }
    }
}

struct PersistedStateRepairStartupSettlementError {
    context: &'static str,
    source: Box<dyn std::error::Error + Send + Sync>,
    preservation: Option<AnchoredRecordQuarantinePreservationError>,
}

impl std::fmt::Debug for PersistedStateRepairStartupSettlementError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PersistedStateRepairStartupSettlementError")
            .field("context", &self.context)
            .field("preservation_pending", &self.preservation.is_some())
            .finish_non_exhaustive()
    }
}

impl std::fmt::Display for PersistedStateRepairStartupSettlementError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.context)?;
        if self.preservation.is_some() {
            formatter.write_str(" while quarantine preservation remained unsettled")?;
        }
        Ok(())
    }
}

impl std::error::Error for PersistedStateRepairStartupSettlementError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.source.as_ref())
    }
}

fn startup_settlement_error(
    context: &'static str,
    source: impl std::error::Error + Send + Sync + 'static,
    preservation: Option<AnchoredRecordQuarantinePreservationError>,
) -> io::Error {
    io::Error::other(PersistedStateRepairStartupSettlementError {
        context,
        source: Box::new(source),
        preservation,
    })
}

#[cfg(test)]
pub(crate) struct PersistedStateRepairHandCoverage {
    pub(crate) admission_type: &'static str,
    pub(crate) attempt_type: &'static str,
    pub(crate) terminal_type: &'static str,
    pub(crate) suppression_hours: i64,
    pub(crate) operation_journal_schema: &'static str,
    pub(crate) failure_memory_schema: &'static str,
    pub(crate) terminal_outcomes: [PersistedStateRepairTerminalOutcome; 3],
    pub(crate) stable_key_dimensions: [&'static str; 4],
    pub(crate) max_attempts_per_stable_key_per_suppression_window: usize,
    pub(crate) durability_contract: [&'static str; 4],
    pub(crate) restart_contract: [&'static str; 5],
}

#[cfg(test)]
pub(crate) fn persisted_state_repair_hand_coverage() -> PersistedStateRepairHandCoverage {
    fn leaf<T>() -> &'static str {
        std::any::type_name::<T>()
            .rsplit("::")
            .next()
            .expect("persisted-state repair type name")
    }

    PersistedStateRepairHandCoverage {
        admission_type: leaf::<PersistedStateRepairAdmission>(),
        attempt_type: leaf::<PersistedStateRepairAttempt>(),
        terminal_type: leaf::<PersistedStateRepairTerminal>(),
        suppression_hours: super::contracts::PERSISTED_STATE_REPAIR_SUPPRESSION_HOURS,
        operation_journal_schema: super::journals::OPERATION_JOURNAL_SCHEMA,
        failure_memory_schema: super::failure_memory::FAILURE_MEMORY_SCHEMA,
        terminal_outcomes: [
            PersistedStateRepairTerminalOutcome::Quarantined,
            PersistedStateRepairTerminalOutcome::Refused,
            PersistedStateRepairTerminalOutcome::AppliedUnverified,
        ],
        stable_key_dimensions: ["store", "record_id", "physical_identity", "mode"],
        max_attempts_per_stable_key_per_suppression_window:
            super::contracts::PERSISTED_STATE_REPAIR_MAX_ATTEMPTS_PER_STABLE_KEY_PER_SUPPRESSION_WINDOW,
        durability_contract: [
            "plan_before_effect",
            "terminal_before_memory",
            "exact_attempt_terminal_binding",
            "immediate_suppression_after_terminal",
        ],
        restart_contract: [
            "nonterminal_without_exact_applied_proof_fail_closed",
            "exact_applied_nonterminal_reconstructed",
            "duplicate_active_key_fail_closed",
            "orphan_active_memory_fail_closed",
            "exact_missing_memory_rebuilt",
        ],
    }
}

pub(crate) struct PersistedStateRejectedRecordQuarantineAuthorization {
    eligibility: PersistedStateRejectedRecordEligibility,
}

impl PersistedStateRejectedRecordQuarantineAuthorization {
    fn eligibility(&self) -> &PersistedStateRejectedRecordEligibility {
        &self.eligibility
    }

    fn still_current(&self) -> bool {
        self.eligibility.still_current()
    }

    fn quarantine(
        self,
        suffix: [u8; 16],
    ) -> Result<
        PersistedStateRejectedRecordQuarantineReceipt,
        crate::execution::anchored_record::AnchoredRecordQuarantineError,
    > {
        self.eligibility.quarantine(suffix)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PersistedStateRepairAuthorizationRejection {
    InvalidAssessment,
    RecordIdentityChanged,
}

pub(crate) struct PersistedStateRepairAdmission {
    authorization: PersistedStateRejectedRecordQuarantineAuthorization,
    attempt: PersistedStateRepairAttempt,
    reservation: PersistedStateRepairReservation,
    config_guard: OwnedMutexGuard<()>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PersistedStateRepairAdmissionRejection {
    ConfigUnavailable,
    ForeignState,
    ModeChanged,
    RecordIdentityChanged,
    Suppressed,
    AmbiguousPriorAttempt,
    PersistencePending,
    AlreadyReserved,
    CapacityExhausted,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum PersistedStateRepairExecutionError {
    #[error("persisted-state repair plan could not be committed")]
    Plan(#[source] OperationJournalStoreError),
    #[error("persisted-state repair completed after an accepted journal persistence failure")]
    AcceptedJournalPersistence(#[source] OperationJournalStoreError),
    #[error("persisted-state repair terminal could not be committed")]
    Terminal {
        #[source]
        source: OperationJournalStoreError,
        preservation: Option<AnchoredRecordQuarantinePreservationError>,
    },
    #[error("persisted-state repair failure memory could not be committed")]
    Memory {
        #[source]
        source: FailureMemoryStoreError,
        preservation: Option<AnchoredRecordQuarantinePreservationError>,
    },
    #[error("persisted-state repair quarantine task did not complete")]
    QuarantineTask(#[source] io::Error),
    #[error("persisted-state repair quarantine preservation remains unsettled")]
    Preservation(#[source] AnchoredRecordQuarantinePreservationError),
}

impl AppState {
    pub(crate) async fn reconcile_persisted_state_repair_startup(&self) -> io::Result<()> {
        self.failure_memory
            .settle_reconciliation_pending()
            .await
            .map_err(|error| {
                io::Error::other(format!(
                    "persisted-state repair memory settlement failed: {}",
                    error.class()
                ))
            })?;
        for journal in self.journals.list() {
            let Some(attempt) = journal.persisted_state_repair_attempt() else {
                continue;
            };
            if journal.persisted_state_repair_terminal().is_some() {
                continue;
            }
            let directory = self
                .persisted_state_repair_directories
                .for_store(attempt.store());
            let original_leaf = super::persisted_state_load::persisted_state_record_name(
                attempt.store(),
                attempt.record_id(),
            )?;
            let recovery_attempt = attempt.clone();
            let (outcome, preservation_failure): (
                PersistedStateRepairTerminalOutcome,
                Option<AnchoredRecordQuarantinePreservationError>
            ) = tokio::task::spawn_blocking(move || {
                let receipt = super::persisted_state_load::admit_exact_applied_persisted_state_quarantine(
                    &directory,
                    original_leaf,
                    &recovery_attempt,
                )?
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "nonterminal persisted-state repair is ambiguous after restart",
                    )
                })?;
                if !receipt.is_current() {
                    return Ok((
                        PersistedStateRepairTerminalOutcome::AppliedUnverified,
                        receipt.acknowledge_applied_unverified(),
                    ));
                }
                Ok(match receipt.acknowledge_preserved() {
                    Ok(()) => {
                        (PersistedStateRepairTerminalOutcome::Quarantined, None)
                    }
                    Err(error) => {
                        (
                            PersistedStateRepairTerminalOutcome::AppliedUnverified,
                            Some(error),
                        )
                    }
                })
            })
            .await
            .map_err(|error| io::Error::other(format!(
                "persisted-state restart quarantine task failed: {error}"
            )))??;
            let terminal = PersistedStateRepairTerminal::from_attempt(
                attempt.clone(),
                outcome,
            );
            if let Err(error) =
                settle_persisted_state_repair_terminal(self.journals.as_ref(), attempt, &terminal)
                    .await
            {
                return Err(startup_settlement_error(
                    "persisted-state restart terminal commit failed",
                    error,
                    preservation_failure,
                ));
            }
            let key = FailureMemoryKey::for_persisted_state_repair(attempt);
            let memory = GuardianFailureMemoryEntry::for_persisted_state_repair_terminal(
                terminal,
            );
            match self.failure_memory.get(&key) {
                Some(current) if current == memory => {}
                Some(_) => {
                    return Err(startup_settlement_error(
                        "persisted-state restart memory conflicts with reconstructed terminal",
                        io::Error::new(
                            io::ErrorKind::InvalidData,
                            "reconstructed memory does not match persisted memory",
                        ),
                        preservation_failure,
                    ));
                }
                None => {
                    let reservation = match self
                        .failure_memory
                        .reserve_persisted_state_repair(attempt)
                    {
                        Ok(reservation) => reservation,
                        Err(_) => {
                            return Err(startup_settlement_error(
                                "persisted-state startup memory reservation was refused",
                                io::Error::new(
                                    io::ErrorKind::WouldBlock,
                                    "persisted-state repair reservation is unavailable",
                                ),
                                preservation_failure,
                            ));
                        }
                    };
                    if let Err(error) = self.failure_memory
                        .record_persisted_state_repair_terminal(memory, &reservation)
                        .await
                    {
                        return Err(startup_settlement_error(
                            "persisted-state startup memory commit failed",
                            error,
                            preservation_failure,
                        ));
                    }
                }
            }
            if let Some(error) = preservation_failure {
                return Err(io::Error::other(error));
            }
        }
        let now = Utc::now();
        let journals = self.journals.list();
        let mut active = BTreeMap::new();
        for journal in &journals {
            let Some(attempt) = journal.persisted_state_repair_attempt() else {
                continue;
            };
            let Some(terminal) = journal.persisted_state_repair_terminal().cloned() else {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "nonterminal persisted-state repair remained unsettled after recovery",
                ));
            };
            if !DateTime::parse_from_rfc3339(terminal.suppression_until())
                .is_ok_and(|until| until > now)
            {
                continue;
            }
            let key = FailureMemoryKey::for_persisted_state_repair(attempt);
            if active
                .insert(key.as_str().to_string(), (key, terminal))
                .is_some()
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "overlapping persisted-state repair terminals share one memory key",
                ));
            }
        }
        for memory in self.failure_memory.list() {
            let Some(terminal) = memory.persisted_state_repair_terminal() else {
                continue;
            };
            if !DateTime::parse_from_rfc3339(terminal.suppression_until())
                .is_ok_and(|until| until > now)
            {
                continue;
            }
            let canonical =
                GuardianFailureMemoryEntry::for_persisted_state_repair_terminal(terminal.clone());
            if canonical != memory
                || !journals.iter().any(|journal| {
                    &journal.operation_id == terminal.operation_id()
                        && journal.persisted_state_repair_terminal() == Some(terminal)
                })
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "active persisted-state repair memory has no exact journal terminal",
                ));
            }
        }
        for (_, (key, terminal)) in active {
            let attempt = terminal.attempt().clone();
            let memory = GuardianFailureMemoryEntry::for_persisted_state_repair_terminal(terminal);
            if self.failure_memory.get(&key).as_ref() == Some(&memory) {
                continue;
            }
            if self.failure_memory.get(&key).is_some() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "active persisted-state repair memory conflicts with its journal terminal",
                ));
            }
            let reservation = self
                .failure_memory
                .reserve_persisted_state_repair(&attempt)
                .map_err(|_| {
                    io::Error::new(
                        io::ErrorKind::WouldBlock,
                        "persisted-state startup memory reservation was refused",
                    )
                })?;
            self.failure_memory
                .record_persisted_state_repair_terminal(memory, &reservation)
                .await
                .map_err(|error| {
                    io::Error::other(format!(
                        "persisted-state startup memory commit failed: {}",
                        error.class()
                    ))
                })?;
        }
        Ok(())
    }

    pub(crate) async fn admit_persisted_state_repair(
        &self,
        authorization: PersistedStateRejectedRecordQuarantineAuthorization,
    ) -> Result<PersistedStateRepairAdmission, PersistedStateRepairAdmissionRejection> {
        if !authorization
            .eligibility()
            .belongs_to(self.persisted_state_rejection_streaks.repair_owner())
        {
            return Err(PersistedStateRepairAdmissionRejection::ForeignState);
        }
        let config_guard = self
            .config
            .acquire_mutation()
            .await
            .map_err(|_| PersistedStateRepairAdmissionRejection::ConfigUnavailable)?;
        if GuardianMode::from_config(&self.config.current().guardian_mode) != GuardianMode::Managed
        {
            return Err(PersistedStateRepairAdmissionRejection::ModeChanged);
        }
        let authorization = tokio::task::spawn_blocking(move || {
            authorization
                .still_current()
                .then_some(authorization)
        })
        .await
        .map_err(|_| PersistedStateRepairAdmissionRejection::RecordIdentityChanged)?
        .ok_or(PersistedStateRepairAdmissionRejection::RecordIdentityChanged)?;

        let eligibility = authorization.eligibility();
        if eligibility.record_target()
            != &super::persisted_state_load::persisted_state_record_target(
                eligibility.store(),
                eligibility.record_id(),
            )
        {
            return Err(PersistedStateRepairAdmissionRejection::RecordIdentityChanged);
        }
        let observed_at = Utc::now().fixed_offset();
        let attempt = PersistedStateRepairAttempt::new(
            eligibility.store(),
            eligibility.record_id(),
            eligibility.physical_identity().clone(),
            GuardianMode::Managed,
            observed_at.to_rfc3339(),
        );
        attempt
            .validate()
            .map_err(|_| PersistedStateRepairAdmissionRejection::RecordIdentityChanged)?;
        let key = FailureMemoryKey::for_persisted_state_repair(&attempt);
        if self.failure_memory.get(&key).is_some_and(|entry| {
            entry
                .persisted_state_repair_terminal()
                .and_then(|terminal| {
                    DateTime::parse_from_rfc3339(terminal.suppression_until()).ok()
                })
                .is_some_and(|until| until > observed_at)
        }) {
            return Err(PersistedStateRepairAdmissionRejection::Suppressed);
        }
        let mut active_terminals = 0usize;
        for journal in self.journals.list() {
            let Some(prior) = journal.persisted_state_repair_attempt() else {
                continue;
            };
            if FailureMemoryKey::for_persisted_state_repair(prior) != key {
                continue;
            }
            match journal.persisted_state_repair_terminal() {
                None => {
                    return Err(PersistedStateRepairAdmissionRejection::AmbiguousPriorAttempt);
                }
                Some(terminal)
                    if DateTime::parse_from_rfc3339(terminal.suppression_until())
                        .is_ok_and(|until| until > observed_at) =>
                {
                    active_terminals += 1;
                }
                Some(_) => {}
            }
        }
        if active_terminals > 1 {
            return Err(PersistedStateRepairAdmissionRejection::AmbiguousPriorAttempt);
        }
        if active_terminals
            >= super::contracts::PERSISTED_STATE_REPAIR_MAX_ATTEMPTS_PER_STABLE_KEY_PER_SUPPRESSION_WINDOW
        {
            return Err(PersistedStateRepairAdmissionRejection::Suppressed);
        }
        let reservation = self
            .failure_memory
            .reserve_persisted_state_repair(&attempt)
            .map_err(|error| match error {
                PersistedStateRepairReserveError::InvalidAttempt => {
                    PersistedStateRepairAdmissionRejection::RecordIdentityChanged
                }
                PersistedStateRepairReserveError::PersistencePending => {
                    PersistedStateRepairAdmissionRejection::PersistencePending
                }
                PersistedStateRepairReserveError::AlreadyReserved => {
                    PersistedStateRepairAdmissionRejection::AlreadyReserved
                }
                PersistedStateRepairReserveError::Suppressed => {
                    PersistedStateRepairAdmissionRejection::Suppressed
                }
                PersistedStateRepairReserveError::CapacityExhausted => {
                    PersistedStateRepairAdmissionRejection::CapacityExhausted
                }
            })?;

        Ok(PersistedStateRepairAdmission {
            authorization,
            attempt,
            reservation,
            config_guard,
        })
    }

    pub(crate) async fn execute_persisted_state_repair(
        &self,
        admission: PersistedStateRepairAdmission,
    ) -> Result<PersistedStateRepairTerminalOutcome, PersistedStateRepairExecutionError> {
        let PersistedStateRepairAdmission {
            authorization,
            attempt,
            reservation,
            config_guard,
        } = admission;
        let mut accepted_journal_error = None;
        match self
            .journals
            .create_persisted_state_repair_plan(attempt.clone())
            .await
        {
            Ok(()) => {}
            Err(error @ OperationJournalStoreError::Persistence(_)) => {
                match self
                    .journals
                    .reconcile_transition(
                        attempt.operation_id(),
                        error,
                        PERSISTED_STATE_REPAIR_JOURNAL_RETRY_INITIAL,
                        PERSISTED_STATE_REPAIR_JOURNAL_RETRY_MAX,
                        |entry| persisted_state_repair_plan_is_visible(entry, &attempt),
                    )
                    .await
                {
                    Ok(OperationJournalReconciliation::CommittedAfterPersistenceFailure(error)) => {
                        accepted_journal_error = Some(error);
                    }
                    Ok(OperationJournalReconciliation::RequestedTransitionAlreadyCommitted) => {}
                    Ok(OperationJournalReconciliation::RetryRequestedTransition) => {
                        return Err(PersistedStateRepairExecutionError::Plan(
                            OperationJournalStoreError::RetryRequired,
                        ));
                    }
                    Err(error) => return Err(PersistedStateRepairExecutionError::Plan(error)),
                }
            }
            Err(error) => return Err(PersistedStateRepairExecutionError::Plan(error)),
        }

        let suffix = persisted_state_repair_quarantine_suffix(&attempt);
        let (outcome, preservation_failure): (
            PersistedStateRepairTerminalOutcome,
            Option<AnchoredRecordQuarantinePreservationError>
        ) = tokio::task::spawn_blocking(move || {
            if !authorization.still_current() {
                return (PersistedStateRepairTerminalOutcome::Refused, None);
            }
            match authorization.quarantine(suffix) {
                Ok(receipt) => {
                    if !receipt.is_current() {
                        return (
                            PersistedStateRepairTerminalOutcome::AppliedUnverified,
                            receipt.acknowledge_applied_unverified(),
                        );
                    }
                    match receipt.acknowledge_preserved() {
                        Ok(()) => {
                            (PersistedStateRepairTerminalOutcome::Quarantined, None)
                        }
                        Err(error) => {
                            (
                                PersistedStateRepairTerminalOutcome::AppliedUnverified,
                                Some(error),
                            )
                        }
                    }
                }
                Err(error @ crate::execution::anchored_record::AnchoredRecordQuarantineError::Refused(_)) => {
                    drop(error);
                    (PersistedStateRepairTerminalOutcome::Refused, None)
                }
                Err(error) => (
                    PersistedStateRepairTerminalOutcome::AppliedUnverified,
                    error.into_preservation_error(),
                ),
            }
        })
        .await
        .map_err(|error| {
            PersistedStateRepairExecutionError::QuarantineTask(io::Error::other(format!(
                "persisted-state quarantine worker failed: {error}"
            )))
        })?;
        let terminal = PersistedStateRepairTerminal::from_attempt(attempt.clone(), outcome);
        match settle_persisted_state_repair_terminal(self.journals.as_ref(), &attempt, &terminal)
            .await
        {
            Ok(Some(error)) => {
                accepted_journal_error.get_or_insert(error);
            }
            Ok(None) => {}
            Err(error) => {
                drop(reservation);
                drop(config_guard);
                return Err(PersistedStateRepairExecutionError::Terminal {
                    source: error,
                    preservation: preservation_failure,
                });
            }
        }

        let memory = GuardianFailureMemoryEntry::for_persisted_state_repair_terminal(terminal);
        if let Err(error) =
            settle_persisted_state_repair_memory(self.failure_memory.as_ref(), memory, &reservation)
                .await
        {
            drop(reservation);
            drop(config_guard);
            return Err(PersistedStateRepairExecutionError::Memory {
                source: error,
                preservation: preservation_failure,
            });
        }
        drop(reservation);
        drop(config_guard);
        if let Some(error) = preservation_failure {
            return Err(PersistedStateRepairExecutionError::Preservation(error));
        }
        if let Some(error) = accepted_journal_error {
            return Err(PersistedStateRepairExecutionError::AcceptedJournalPersistence(error));
        }
        Ok(outcome)
    }
}

async fn settle_persisted_state_repair_terminal(
    journals: &super::journals::OperationJournalStore,
    attempt: &PersistedStateRepairAttempt,
    terminal: &PersistedStateRepairTerminal,
) -> Result<Option<OperationJournalStoreError>, OperationJournalStoreError> {
    match journals
        .record_persisted_state_repair_terminal(attempt.operation_id(), terminal.clone())
        .await
    {
        Ok(()) => Ok(None),
        Err(error) => match journals
            .reconcile_transition(
                attempt.operation_id(),
                error,
                PERSISTED_STATE_REPAIR_JOURNAL_RETRY_INITIAL,
                PERSISTED_STATE_REPAIR_JOURNAL_RETRY_MAX,
                |entry| persisted_state_repair_terminal_is_visible(entry, terminal),
            )
            .await?
        {
            OperationJournalReconciliation::CommittedAfterPersistenceFailure(error) => {
                Ok(Some(error))
            }
            OperationJournalReconciliation::RequestedTransitionAlreadyCommitted => Ok(None),
            OperationJournalReconciliation::RetryRequestedTransition => {
                Err(OperationJournalStoreError::RetryRequired)
            }
        },
    }
}

async fn settle_persisted_state_repair_memory(
    failure_memory: &super::failure_memory::GuardianFailureMemoryStore,
    memory: GuardianFailureMemoryEntry,
    reservation: &PersistedStateRepairReservation,
) -> Result<(), FailureMemoryStoreError> {
    let mut error = match failure_memory
        .record_persisted_state_repair_terminal(memory.clone(), reservation)
        .await
    {
        Ok(()) => return Ok(()),
        Err(error @ FailureMemoryStoreError::Persistence(_)) => error,
        Err(error) => return Err(error),
    };
    let mut delay = PERSISTED_STATE_REPAIR_JOURNAL_RETRY_INITIAL;
    for attempt in 0..PERSISTED_STATE_REPAIR_MEMORY_SETTLEMENT_ATTEMPTS {
        match failure_memory.settle_reconciliation_pending().await {
            Ok(()) => {}
            Err(next @ FailureMemoryStoreError::Persistence(_)) => error = next,
            Err(next) => return Err(next),
        }
        if failure_memory.get(&memory.key).as_ref() == Some(&memory) {
            return Ok(());
        }
        if attempt + 1 < PERSISTED_STATE_REPAIR_MEMORY_SETTLEMENT_ATTEMPTS {
            tokio::time::sleep(delay).await;
            delay = delay
                .saturating_mul(2)
                .min(PERSISTED_STATE_REPAIR_JOURNAL_RETRY_MAX);
        }
    }
    Err(error)
}

pub(crate) fn authorize_persisted_state_rejected_record_quarantine(
    eligibility: PersistedStateRejectedRecordEligibility,
    proof: PersistedStateRepairAssessmentProof,
    decision: &GuardianDecision,
) -> Result<
    PersistedStateRejectedRecordQuarantineAuthorization,
    PersistedStateRepairAuthorizationRejection,
> {
    if proof.assessed_mode() != GuardianMode::Managed || !exact_managed_decision(decision) {
        return Err(PersistedStateRepairAuthorizationRejection::InvalidAssessment);
    }
    Ok(PersistedStateRejectedRecordQuarantineAuthorization { eligibility })
}

fn exact_managed_decision(decision: &GuardianDecision) -> bool {
    if decision.operation_id().is_some()
        || decision.mode() != GuardianMode::Managed
        || decision.kind() != GuardianActionKind::Quarantine
        || decision.diagnoses() != [DiagnosisId::PersistedStateSchemaInvalid]
    {
        return false;
    }
    let Some(plan) = decision.action_plan() else {
        return false;
    };
    if plan.owner != StabilizationSystem::Guardian
        || plan.prerequisite.diagnosis_id != DiagnosisId::PersistedStateSchemaInvalid
        || plan.prerequisite.ownership != OwnershipClass::LauncherManaged
        || plan.prerequisite.confidence != crate::guardian::GuardianConfidence::Confirmed
        || plan.prerequisite.candidate_actions != PERSISTED_STATE_REPAIR_CANDIDATES
        || plan.prerequisite.affected_targets.len() != 1
        || plan.actions.len() != 1
    {
        return false;
    }
    let target = &plan.prerequisite.affected_targets[0];
    let action = &plan.actions[0];
    exact_target(target)
        && action.kind == GuardianActionKind::Quarantine
        && action.reason == DiagnosisId::PersistedStateSchemaInvalid
        && action.target.as_ref() == Some(target)
}

fn exact_target(target: &crate::state::contracts::TargetDescriptor) -> bool {
    target == &super::persisted_state_load_target()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execution::persistence::{AtomicWriteBackend, PersistenceCoordinator};
    use crate::guardian::persisted_state_repair::{
        PersistedStateRepairDisposition, assess_persisted_state_repair,
    };
    use crate::state::failure_memory::{FailureMemorySnapshot, GuardianFailureMemoryStore};
    use crate::state::journals::{OperationJournalSnapshot, OperationJournalStore};
    use crate::state::{
        AppStateInit, InstallStore, PersistedStateRejectedRecordEligibility, SessionStore,
        persisted_state_rejected_record_eligibility_for_test,
    };
    use axial_config::{AppConfig, AppPaths, InstanceRegistrySnapshot};
    use static_assertions::assert_not_impl_any;
    use std::ffi::OsStr;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

    struct PermanentFailureBackend {
        attempts: AtomicUsize,
    }

    impl AtomicWriteBackend for PermanentFailureBackend {
        fn write(
            &self,
            _destination: &crate::execution::anchored_record::AnchoredRecordTarget,
            _effects: &axial_fs::EffectOwner,
            _contents: &[u8],
        ) -> io::Result<()> {
            self.attempts.fetch_add(1, Ordering::SeqCst);
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "permanent injected failure-memory write failure",
            ))
        }
    }

    assert_not_impl_any!(
        PersistedStateRepairAdmission:
            Clone,
            std::fmt::Debug,
            serde::Serialize,
            serde::de::DeserializeOwned,
            AsRef<Path>,
            AsRef<[u8]>
    );

    struct Fixture {
        state: AppState,
        root: PathBuf,
    }

    fn fixture(label: &str) -> Fixture {
        static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(1);
        let root = std::env::temp_dir().join(format!(
            "axial-persisted-state-repair-{label}-{}-{}",
            std::process::id(),
            NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&root);
        let paths = AppPaths::from_root(root.to_path_buf()).expect("absolute test app root");
        fs::create_dir_all(
            paths
                .config_file()
                .parent()
                .expect("config path has a parent"),
        )
        .expect("app root");
        let root_session = crate::state::test_root_session(&paths);
        let config = Arc::new(
            axial_config::ConfigStore::load_from(paths.clone(), Arc::clone(&root_session))
                .expect("test config"),
        );
        let instances = Arc::new(
            axial_config::InstanceStore::from_snapshot(
                paths.clone(),
                root_session,
                InstanceRegistrySnapshot::default(),
            )
            .expect("test instances"),
        );
        let state = AppState::new(AppStateInit {
            app_name: "Axial".to_string(),
            version: "test".to_string(),
            config,
            instances,
            installs: Arc::new(InstallStore::new()),
            sessions: Arc::new(SessionStore::new()),
            performance: Arc::new(
                axial_performance::PerformanceManager::load_for_startup(paths.performance_dir())
                    .expect("test performance state"),
            ),
            startup_warnings: Vec::new(),
        });
        Fixture { state, root }
    }

    fn record_id(index: u128) -> String {
        crate::state::contracts::OperationId::deterministic_test(format!("record-{index}"))
            .to_string()
    }

    fn owned_eligibility(
        state: &AppState,
        root: &Path,
        index: u128,
    ) -> (PathBuf, PersistedStateRejectedRecordEligibility) {
        let record_root = root.join(format!("rejected-{index}"));
        fs::create_dir_all(&record_root).expect("rejected-record root");
        let id = record_id(index);
        let file_name = format!("{id}.json");
        let path = record_root.join(&file_name);
        fs::write(&path, b"{").expect("rejected record");
        let eligibility = persisted_state_rejected_record_eligibility_for_test(
            &record_root,
            OsStr::new(&file_name),
            &id,
        )
        .expect("exact rejected-record eligibility")
        .bind_owner_for_test(
            state
                .persisted_state_rejection_streaks
                .repair_owner()
                .clone(),
        );
        (path, eligibility)
    }

    fn managed_authorization(
        eligibility: PersistedStateRejectedRecordEligibility,
    ) -> PersistedStateRejectedRecordQuarantineAuthorization {
        match assess_persisted_state_repair(GuardianMode::Managed, eligibility) {
            PersistedStateRepairDisposition::Managed(managed) => managed.into_authorization(),
            PersistedStateRepairDisposition::NoEffect => {
                panic!("Managed assessment must authorize exact quarantine")
            }
        }
    }

    #[tokio::test]
    async fn managed_repair_quarantines_and_commits_exact_terminal_and_suppression_memory() {
        let fixture = fixture("managed-e2e");
        let (record_path, eligibility) = owned_eligibility(&fixture.state, &fixture.root, 1);
        let admission = fixture
            .state
            .admit_persisted_state_repair(managed_authorization(eligibility))
            .await
            .expect("exact Managed admission");

        assert_eq!(
            fixture
                .state
                .execute_persisted_state_repair(admission)
                .await
                .expect("durable persisted-state repair"),
            PersistedStateRepairTerminalOutcome::Quarantined
        );
        assert!(!record_path.exists());

        let journals = fixture.state.journals().list();
        assert_eq!(journals.len(), 1);
        let terminal = journals[0]
            .persisted_state_repair_terminal()
            .expect("typed persisted-state terminal");
        assert_eq!(
            terminal.outcome(),
            PersistedStateRepairTerminalOutcome::Quarantined
        );
        assert_eq!(
            journals[0].persisted_state_repair_attempt(),
            Some(terminal.attempt())
        );
        let key = FailureMemoryKey::for_persisted_state_repair(terminal.attempt());
        let memory = fixture
            .state
            .failure_memory()
            .get(&key)
            .expect("immediate exact suppression memory");
        assert_eq!(memory.persisted_state_repair_terminal(), Some(terminal));
        assert!(
            DateTime::parse_from_rfc3339(terminal.suppression_until())
                .is_ok_and(|until| until > Utc::now())
        );

        let suffix = persisted_state_repair_quarantine_suffix(terminal.attempt())
            .into_iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        assert_eq!(suffix.len(), 32);
        let quarantine = record_path.parent().expect("record parent").join(format!(
            ".{}.axial-quarantine-{suffix}",
            record_path
                .file_name()
                .expect("record file name")
                .to_string_lossy()
        ));
        assert_eq!(fs::read(quarantine).expect("quarantined bytes"), b"{");

        drop(fixture.state);
        let _ = fs::remove_dir_all(fixture.root);
    }

    #[cfg(unix)]
    #[test]
    fn reconstructed_quarantine_rechecks_aliases_before_acknowledgement() {
        let fixture = fixture("restart-quarantine-alias");
        let id = record_id(18);
        let source = super::super::persisted_state_load::persisted_state_record_path(
            fixture.state.config().paths(),
            super::super::contracts::PersistedStateRecordStore::PerformanceOperation,
            &id,
        );
        let parent = source.parent().expect("canonical record parent");
        fs::create_dir_all(parent).expect("canonical record directory");
        fs::write(&source, b"{").expect("canonical rejected record");
        let eligibility = persisted_state_rejected_record_eligibility_for_test(
            parent,
            source.file_name().expect("canonical record name"),
            &id,
        )
        .expect("canonical rejected-record eligibility");
        let attempt = PersistedStateRepairAttempt::new(
            eligibility.store(),
            eligibility.record_id(),
            eligibility.physical_identity().clone(),
            GuardianMode::Managed,
            Utc::now().fixed_offset().to_rfc3339(),
        );
        let suffix = persisted_state_repair_quarantine_suffix(&attempt);
        let receipt = eligibility
            .quarantine(suffix)
            .unwrap_or_else(|_| panic!("apply exact quarantine"));
        receipt
            .acknowledge_preserved()
            .expect("settle pre-restart quarantine");

        let directory = AnchoredRecordDirectory::for_test_directory(parent)
            .expect("reopen quarantine directory");
        let original_leaf = axial_fs::LeafName::new(
            source.file_name().expect("canonical record leaf").to_os_string(),
        )
        .expect("canonical record leaf");
        let receipt =
            super::super::persisted_state_load::admit_exact_applied_persisted_state_quarantine(
                &directory,
                original_leaf.clone(),
                &attempt,
            )
            .expect("reconstruct exact quarantine")
            .expect("applied quarantine exists");
        let parked = crate::execution::anchored_record::anchored_record_quarantine_name(
            original_leaf.as_os_str(),
            suffix,
        );
        let alias = parked
            .to_str()
            .expect("portable quarantine leaf")
            .to_ascii_uppercase();
        let mut alias = match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(parent.join(alias))
        {
            Ok(alias) => alias,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                receipt
                    .acknowledge_preserved()
                    .expect("settle restart quarantine on a case-insensitive filesystem");
                let root = fixture.root.clone();
                drop((directory, fixture));
                let _ = fs::remove_dir_all(root);
                return;
            }
            Err(error) => panic!("inject restart alias: {error}"),
        };
        std::io::Write::write_all(&mut alias, b"alias").expect("write restart alias");
        drop(alias);

        assert!(!receipt.is_current());
        assert!(matches!(
            receipt.acknowledge_preserved(),
            Err(AnchoredRecordQuarantinePreservationError::Alias { .. })
        ));
        let root = fixture.root.clone();
        drop((directory, fixture));
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn permanent_memory_failure_is_bounded_retained_and_blocks_the_startup_barrier() {
        let fixture = fixture("permanent-memory-failure");
        let backend = Arc::new(PermanentFailureBackend {
            attempts: AtomicUsize::new(0),
        });
        let failure_memory = Arc::new(
            GuardianFailureMemoryStore::try_load_from_paths_with_coordinator(
                fixture.state.config().paths(),
                PersistenceCoordinator::for_test(
                    backend.clone(),
                    Duration::from_millis(1),
                    Duration::from_millis(2),
                ),
            )
            .expect("persistent failure-memory fixture"),
        );
        let state = fixture.state.clone().with_reconciliation_stores(
            Arc::new(OperationJournalStore::new()),
            failure_memory.clone(),
        );
        let (source, eligibility) = owned_eligibility(&state, &fixture.root, 10);
        state.publish_persisted_state_repair_eligibilities_for_test(vec![eligibility]);

        let settled = tokio::time::timeout(
            Duration::from_secs(2),
            crate::application::settle_startup_persisted_state_repairs(&state),
        )
        .await
        .expect("bounded startup repair barrier");
        assert!(!settled);
        assert!(!source.exists(), "effect was physically applied");
        let journal = state
            .journals()
            .list()
            .into_iter()
            .find(|entry| entry.persisted_state_repair_attempt().is_some())
            .expect("exact terminal journal");
        let attempt = journal
            .persisted_state_repair_attempt()
            .expect("persisted-state attempt");
        assert!(journal.persisted_state_repair_terminal().is_some());
        assert!(failure_memory.list().is_empty());
        assert_eq!(
            failure_memory.reserve_persisted_state_repair(attempt).err(),
            Some(PersistedStateRepairReserveError::PersistencePending),
            "exact failed memory candidate remains owned"
        );
        assert!(
            backend.attempts.load(Ordering::SeqCst)
                > PERSISTED_STATE_REPAIR_MEMORY_SETTLEMENT_ATTEMPTS
        );
        let config_guard = tokio::time::timeout(
            Duration::from_millis(100),
            state.config().acquire_mutation(),
        )
        .await
        .expect("bounded repair releases config mutation guard")
        .expect("config mutation remains available");
        drop(config_guard);

        let root = fixture.root.clone();
        drop((state, failure_memory, fixture.state));
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn restart_reconstructs_an_exact_applied_quarantine_without_replay() {
        let fixture = fixture("exact-applied-restart");
        let id = record_id(8);
        let source = super::super::persisted_state_load::persisted_state_record_path(
            fixture.state.config().paths(),
            super::super::contracts::PersistedStateRecordStore::PerformanceOperation,
            &id,
        );
        let parent = source.parent().expect("canonical record parent");
        fs::create_dir_all(parent).expect("canonical record directory");
        fs::write(&source, b"{").expect("canonical rejected record");
        let eligibility = persisted_state_rejected_record_eligibility_for_test(
            parent,
            source.file_name().expect("canonical record name"),
            &id,
        )
        .expect("canonical rejected-record eligibility");
        let attempt = PersistedStateRepairAttempt::new(
            eligibility.store(),
            eligibility.record_id(),
            eligibility.physical_identity().clone(),
            GuardianMode::Managed,
            Utc::now().fixed_offset().to_rfc3339(),
        );
        fixture
            .state
            .journals()
            .create_persisted_state_repair_plan(attempt.clone())
            .await
            .expect("durable pre-effect plan");
        let suffix = persisted_state_repair_quarantine_suffix(&attempt);
        let receipt = match eligibility.quarantine(suffix) {
            Ok(receipt) => receipt,
            Err(_) => panic!("simulate applied effect before process exit"),
        };
        assert!(receipt.is_current());
        assert!(!source.exists());
        receipt
            .acknowledge_preserved()
            .expect("simulate process boundary after preserved park");

        fixture
            .state
            .reconcile_persisted_state_repair_startup()
            .await
            .expect("exact applied quarantine is reconstructible");
        let journal = fixture
            .state
            .journals()
            .get(attempt.operation_id())
            .expect("reconstructed journal");
        let terminal = journal
            .persisted_state_repair_terminal()
            .expect("reconstructed exact terminal");
        assert_eq!(
            terminal.outcome(),
            PersistedStateRepairTerminalOutcome::Quarantined
        );
        let key = FailureMemoryKey::for_persisted_state_repair(&attempt);
        assert_eq!(
            fixture
                .state
                .failure_memory()
                .get(&key)
                .and_then(|memory| memory.persisted_state_repair_terminal().cloned()),
            Some(terminal.clone())
        );

        drop(fixture.state);
        let _ = fs::remove_dir_all(fixture.root);
    }

    #[tokio::test]
    async fn expired_same_key_retry_mints_a_new_attempt_and_supersedes_memory() {
        let fixture = fixture("expired-retry");
        let (_, eligibility) = owned_eligibility(&fixture.state, &fixture.root, 7);
        let expired_attempt = PersistedStateRepairAttempt::new(
            eligibility.store(),
            eligibility.record_id(),
            eligibility.physical_identity().clone(),
            GuardianMode::Managed,
            (Utc::now() - chrono::Duration::hours(49))
                .fixed_offset()
                .to_rfc3339(),
        );
        let expired_terminal = PersistedStateRepairTerminal::from_attempt(
            expired_attempt.clone(),
            PersistedStateRepairTerminalOutcome::Refused,
        );
        fixture
            .state
            .journals()
            .create_persisted_state_repair_plan(expired_attempt.clone())
            .await
            .expect("expired plan");
        fixture
            .state
            .journals()
            .record_persisted_state_repair_terminal(
                expired_attempt.operation_id(),
                expired_terminal.clone(),
            )
            .await
            .expect("expired terminal");
        let key = FailureMemoryKey::for_persisted_state_repair(&expired_attempt);
        let reservation = fixture
            .state
            .failure_memory()
            .reserve_persisted_state_repair(&expired_attempt)
            .expect("expired memory reservation");
        fixture
            .state
            .failure_memory()
            .record_persisted_state_repair_terminal(
                GuardianFailureMemoryEntry::for_persisted_state_repair_terminal(expired_terminal),
                &reservation,
            )
            .await
            .expect("expired memory");
        drop(reservation);

        let admission = fixture
            .state
            .admit_persisted_state_repair(managed_authorization(eligibility))
            .await
            .expect("expired suppression permits retry");
        assert_ne!(
            admission.attempt.operation_id(),
            expired_attempt.operation_id()
        );
        let new_operation_id = admission.attempt.operation_id().clone();
        assert_eq!(
            fixture
                .state
                .execute_persisted_state_repair(admission)
                .await
                .expect("retry effect"),
            PersistedStateRepairTerminalOutcome::Quarantined
        );

        let memory = fixture
            .state
            .failure_memory()
            .get(&key)
            .expect("same stable key remains present");
        assert_eq!(
            memory
                .persisted_state_repair_terminal()
                .expect("replacement terminal")
                .operation_id(),
            &new_operation_id
        );
        assert_eq!(
            fixture.state.failure_memory().list().len(),
            1,
            "expired same-key memory must be replaced, not duplicated"
        );
        assert_eq!(
            fixture
                .state
                .journals()
                .list()
                .iter()
                .filter(|entry| entry.persisted_state_repair_terminal().is_some())
                .count(),
            2,
            "durable history retains both unique attempts"
        );

        drop(fixture.state);
        let _ = fs::remove_dir_all(fixture.root);
    }

    #[tokio::test]
    async fn reservation_uses_the_exact_attempt_timestamp_at_the_suppression_boundary() {
        let fixture = fixture("suppression-boundary");
        let (_, eligibility) = owned_eligibility(&fixture.state, &fixture.root, 9);
        let replacement_observed_at = Utc::now().fixed_offset();
        let prior_attempt = PersistedStateRepairAttempt::new(
            eligibility.store(),
            eligibility.record_id(),
            eligibility.physical_identity().clone(),
            GuardianMode::Managed,
            (replacement_observed_at - chrono::Duration::hours(24)
                + chrono::Duration::milliseconds(250))
            .to_rfc3339(),
        );
        let prior_terminal = PersistedStateRepairTerminal::from_attempt(
            prior_attempt.clone(),
            PersistedStateRepairTerminalOutcome::Refused,
        );
        let prior_reservation = fixture
            .state
            .failure_memory()
            .reserve_persisted_state_repair(&prior_attempt)
            .expect("prior reservation");
        fixture
            .state
            .failure_memory()
            .record_persisted_state_repair_terminal(
                GuardianFailureMemoryEntry::for_persisted_state_repair_terminal(prior_terminal),
                &prior_reservation,
            )
            .await
            .expect("prior suppression memory");
        drop(prior_reservation);
        let replacement = PersistedStateRepairAttempt::new(
            eligibility.store(),
            eligibility.record_id(),
            eligibility.physical_identity().clone(),
            GuardianMode::Managed,
            replacement_observed_at.to_rfc3339(),
        );

        tokio::time::sleep(Duration::from_millis(350)).await;
        let rejection = fixture
            .state
            .failure_memory()
            .reserve_persisted_state_repair(&replacement)
            .err();
        assert_eq!(
            rejection,
            Some(PersistedStateRepairReserveError::Suppressed)
        );

        drop((eligibility, fixture.state));
        let _ = fs::remove_dir_all(fixture.root);
    }

    #[tokio::test]
    async fn restart_rebuilds_exact_memory_and_fails_closed_on_ambiguity() {
        let fixture = fixture("restart");
        let (_, eligibility) = owned_eligibility(&fixture.state, &fixture.root, 2);
        let admission = fixture
            .state
            .admit_persisted_state_repair(managed_authorization(eligibility))
            .await
            .expect("Managed admission");
        fixture
            .state
            .execute_persisted_state_repair(admission)
            .await
            .expect("complete seed repair");
        let journal_snapshot = fixture
            .state
            .journals()
            .snapshot()
            .expect("journal snapshot");
        let terminal = journal_snapshot.entries[0]
            .persisted_state_repair_terminal()
            .expect("seed terminal")
            .clone();
        let attempt = terminal.attempt().clone();
        let key = FailureMemoryKey::for_persisted_state_repair(&attempt);

        let rebuilt_journals = Arc::new(OperationJournalStore::new());
        rebuilt_journals
            .load_snapshot(journal_snapshot.clone())
            .expect("restarted journals");
        let rebuilt_memory = Arc::new(GuardianFailureMemoryStore::new());
        let rebuilt_state = fixture
            .state
            .clone()
            .with_reconciliation_stores(rebuilt_journals, rebuilt_memory.clone());
        rebuilt_state
            .reconcile_persisted_state_repair_startup()
            .await
            .expect("exact missing memory is reconstructible");
        assert_eq!(
            rebuilt_memory
                .get(&key)
                .and_then(|entry| entry.persisted_state_repair_terminal().cloned()),
            Some(terminal.clone())
        );

        let nonterminal_journals = Arc::new(OperationJournalStore::new());
        nonterminal_journals
            .create_persisted_state_repair_plan(attempt.clone())
            .await
            .expect("nonterminal plan");
        let nonterminal_state = fixture.state.clone().with_reconciliation_stores(
            nonterminal_journals,
            Arc::new(GuardianFailureMemoryStore::new()),
        );
        assert_eq!(
            nonterminal_state
                .reconcile_persisted_state_repair_startup()
                .await
                .expect_err("nonterminal restart must fail closed")
                .kind(),
            io::ErrorKind::InvalidData
        );

        let duplicate_attempt = PersistedStateRepairAttempt::new(
            attempt.store(),
            attempt.record_id(),
            attempt.physical_identity().clone(),
            GuardianMode::Managed,
            attempt.observed_at(),
        );
        let duplicate_terminal = PersistedStateRepairTerminal::from_attempt(
            duplicate_attempt.clone(),
            PersistedStateRepairTerminalOutcome::Refused,
        );
        let duplicate_journals = Arc::new(OperationJournalStore::new());
        for (attempt, terminal) in [
            (attempt.clone(), terminal.clone()),
            (duplicate_attempt.clone(), duplicate_terminal.clone()),
        ] {
            duplicate_journals
                .create_persisted_state_repair_plan(attempt.clone())
                .await
                .expect("duplicate-key plan");
            duplicate_journals
                .record_persisted_state_repair_terminal(attempt.operation_id(), terminal)
                .await
                .expect("duplicate-key terminal");
        }
        let duplicate_state = fixture.state.clone().with_reconciliation_stores(
            duplicate_journals,
            Arc::new(GuardianFailureMemoryStore::new()),
        );
        assert_eq!(
            duplicate_state
                .reconcile_persisted_state_repair_startup()
                .await
                .expect_err("duplicate active keys must fail closed")
                .kind(),
            io::ErrorKind::InvalidData
        );

        let orphan_memory = Arc::new(GuardianFailureMemoryStore::new());
        orphan_memory
            .load_snapshot(
                FailureMemorySnapshot::new(vec![
                    GuardianFailureMemoryEntry::for_persisted_state_repair_terminal(
                        duplicate_terminal,
                    ),
                ])
                .expect("orphan memory snapshot"),
            )
            .expect("orphan memory");
        let orphan_state = fixture
            .state
            .clone()
            .with_reconciliation_stores(Arc::new(OperationJournalStore::new()), orphan_memory);
        assert_eq!(
            orphan_state
                .reconcile_persisted_state_repair_startup()
                .await
                .expect_err("orphan active memory must fail closed")
                .kind(),
            io::ErrorKind::InvalidData
        );

        let conflict_memory = Arc::new(GuardianFailureMemoryStore::new());
        let conflict_terminal = PersistedStateRepairTerminal::from_attempt(
            attempt.clone(),
            PersistedStateRepairTerminalOutcome::Refused,
        );
        conflict_memory
            .load_snapshot(
                FailureMemorySnapshot::new(vec![
                    GuardianFailureMemoryEntry::for_persisted_state_repair_terminal(
                        conflict_terminal,
                    ),
                ])
                .expect("conflict memory snapshot"),
            )
            .expect("conflict memory");
        let conflict_journals = Arc::new(OperationJournalStore::new());
        conflict_journals
            .load_snapshot(
                OperationJournalSnapshot::new(vec![journal_snapshot.entries[0].clone()])
                    .expect("conflict journal snapshot"),
            )
            .expect("conflict journals");
        let conflict_state = fixture
            .state
            .clone()
            .with_reconciliation_stores(conflict_journals, conflict_memory);
        assert_eq!(
            conflict_state
                .reconcile_persisted_state_repair_startup()
                .await
                .expect_err("conflicting active memory must fail closed")
                .kind(),
            io::ErrorKind::InvalidData
        );

        drop((
            conflict_state,
            orphan_state,
            duplicate_state,
            nonterminal_state,
            rebuilt_state,
            fixture.state,
        ));
        let _ = fs::remove_dir_all(fixture.root);
    }

    #[tokio::test]
    async fn admission_rejects_foreign_state_mode_change_and_replaced_identity() {
        let owner = fixture("owner-binding");
        let foreign = fixture("foreign-binding");
        let (foreign_record, eligibility) = owned_eligibility(&owner.state, &owner.root, 3);
        let rejection = foreign
            .state
            .admit_persisted_state_repair(managed_authorization(eligibility))
            .await
            .err();
        assert_eq!(
            rejection,
            Some(PersistedStateRepairAdmissionRejection::ForeignState)
        );
        assert!(foreign_record.exists());
        assert!(foreign.state.journals().list().is_empty());
        assert!(foreign.state.failure_memory().list().is_empty());

        for (index, mode) in [(4, "custom"), (5, "disabled")] {
            let fixture = fixture(mode);
            let (record, eligibility) = owned_eligibility(&fixture.state, &fixture.root, index);
            let authorization = managed_authorization(eligibility);
            fixture
                .state
                .config()
                .replace_for_test(AppConfig {
                    guardian_mode: mode.to_string(),
                    ..fixture.state.config().current()
                })
                .expect("replace Guardian mode");
            let rejection = fixture
                .state
                .admit_persisted_state_repair(authorization)
                .await
                .err();
            assert_eq!(
                rejection,
                Some(PersistedStateRepairAdmissionRejection::ModeChanged)
            );
            assert!(record.exists());
            assert!(fixture.state.journals().list().is_empty());
            assert!(fixture.state.failure_memory().list().is_empty());
            drop(fixture.state);
            let _ = fs::remove_dir_all(fixture.root);
        }

        let replaced = fixture("identity-replaced");
        let (record, eligibility) = owned_eligibility(&replaced.state, &replaced.root, 6);
        let authorization = managed_authorization(eligibility);
        fs::remove_file(&record).expect("remove observed record");
        fs::write(&record, b"replacement").expect("replace observed record");
        let rejection = replaced
            .state
            .admit_persisted_state_repair(authorization)
            .await
            .err();
        assert_eq!(
            rejection,
            Some(PersistedStateRepairAdmissionRejection::RecordIdentityChanged)
        );
        assert!(replaced.state.journals().list().is_empty());
        assert!(replaced.state.failure_memory().list().is_empty());

        let owner_root = owner.root.clone();
        let foreign_root = foreign.root.clone();
        let replaced_root = replaced.root.clone();
        drop((owner.state, foreign.state, replaced.state));
        let _ = fs::remove_dir_all(owner_root);
        let _ = fs::remove_dir_all(foreign_root);
        let _ = fs::remove_dir_all(replaced_root);
    }

    #[test]
    fn hand_coverage_is_compile_linked_to_exact_durable_contract() {
        let coverage = persisted_state_repair_hand_coverage();
        assert_eq!(coverage.admission_type, "PersistedStateRepairAdmission");
        assert_eq!(coverage.attempt_type, "PersistedStateRepairAttempt");
        assert_eq!(coverage.terminal_type, "PersistedStateRepairTerminal");
        assert_eq!(coverage.suppression_hours, 24);
        assert_eq!(
            coverage.operation_journal_schema,
            "axial.state.operation_journals.v6"
        );
        assert_eq!(
            coverage.failure_memory_schema,
            "axial.guardian.failure_memory.v5"
        );
        assert_eq!(
            coverage.terminal_outcomes,
            [
                PersistedStateRepairTerminalOutcome::Quarantined,
                PersistedStateRepairTerminalOutcome::Refused,
                PersistedStateRepairTerminalOutcome::AppliedUnverified,
            ]
        );
        assert_eq!(
            coverage.stable_key_dimensions,
            ["store", "record_id", "physical_identity", "mode"]
        );
        assert_eq!(
            coverage.max_attempts_per_stable_key_per_suppression_window,
            1
        );
        assert_eq!(
            coverage.durability_contract,
            [
                "plan_before_effect",
                "terminal_before_memory",
                "exact_attempt_terminal_binding",
                "immediate_suppression_after_terminal",
            ]
        );
        assert_eq!(
            coverage.restart_contract,
            [
                "nonterminal_without_exact_applied_proof_fail_closed",
                "exact_applied_nonterminal_reconstructed",
                "duplicate_active_key_fail_closed",
                "orphan_active_memory_fail_closed",
                "exact_missing_memory_rebuilt",
            ]
        );
    }
}
