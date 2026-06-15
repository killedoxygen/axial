use super::contracts::{
    CommandKind, OperationId, OperationJournalEntry, OperationJournalStep, OperationOutcome,
    OperationStatus,
};
use std::collections::BTreeMap;
use std::sync::RwLock;

#[derive(Default)]
pub struct OperationJournalStore {
    records: RwLock<BTreeMap<String, OperationJournalEntry>>,
}

impl OperationJournalStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn create(&self, entry: OperationJournalEntry) {
        if let Ok(mut records) = self.records.write() {
            records.insert(entry.operation_id.as_str().to_string(), entry);
        }
    }

    pub fn get(&self, operation_id: &OperationId) -> Option<OperationJournalEntry> {
        self.records
            .read()
            .ok()
            .and_then(|records| records.get(operation_id.as_str()).cloned())
    }

    pub fn latest_for_command(&self, command: CommandKind) -> Option<OperationJournalEntry> {
        self.records.read().ok().and_then(|records| {
            records
                .values()
                .filter(|entry| entry.command == command)
                .max_by(|left, right| left.operation_id.as_str().cmp(right.operation_id.as_str()))
                .cloned()
        })
    }

    pub fn record_success(
        &self,
        operation_id: &OperationId,
        completed_step: OperationJournalStep,
        outcome: OperationOutcome,
    ) {
        self.update(operation_id, |entry| {
            entry.status = OperationStatus::Succeeded;
            entry.completed_steps.push(completed_step);
            entry.failure_point = None;
            entry.outcome = Some(outcome);
        });
    }

    pub fn record_failure(
        &self,
        operation_id: &OperationId,
        failure_step: OperationJournalStep,
        failure_point: impl Into<String>,
        outcome: OperationOutcome,
    ) {
        self.update(operation_id, |entry| {
            entry.status = OperationStatus::Failed;
            entry.completed_steps.push(failure_step);
            entry.failure_point = Some(failure_point.into());
            entry.outcome = Some(outcome);
        });
    }

    pub fn record_progress(&self, operation_id: &OperationId, progress_step: OperationJournalStep) {
        self.update(operation_id, |entry| {
            entry.status = OperationStatus::Running;
            entry.completed_steps.push(progress_step);
        });
    }

    pub fn record_guardian_evidence(
        &self,
        operation_id: &OperationId,
        fact_ids: Vec<String>,
        diagnosis_ids: Vec<String>,
    ) {
        self.update(operation_id, |entry| {
            if let Some(step) = entry.completed_steps.last_mut() {
                for fact_id in fact_ids {
                    if !step.generated_facts.contains(&fact_id) {
                        step.generated_facts.push(fact_id);
                    }
                }
            }
            for diagnosis_id in diagnosis_ids {
                if !entry.guardian_diagnosis_ids.contains(&diagnosis_id) {
                    entry.guardian_diagnosis_ids.push(diagnosis_id);
                }
            }
        });
    }

    fn update(&self, operation_id: &OperationId, update: impl FnOnce(&mut OperationJournalEntry)) {
        if let Ok(mut records) = self.records.write()
            && let Some(entry) = records.get_mut(operation_id.as_str())
        {
            update(entry);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::OperationJournalStore;
    use crate::state::contracts::{
        CommandKind, JournalId, OperationId, OperationJournalEntry, OperationJournalStep,
        OperationOutcome, OperationPhase, OperationStatus, OwnershipClass, RollbackState,
        StabilizationSystem,
    };

    #[test]
    fn journal_store_creates_updates_and_reads_records() {
        let store = OperationJournalStore::new();
        let operation_id = OperationId::new("operation-1");
        let mut entry = OperationJournalEntry::new(
            JournalId::new("journal-1"),
            operation_id.clone(),
            CommandKind::RefreshPerformanceRules,
            StabilizationSystem::Application,
            OwnershipClass::LauncherManaged,
            RollbackState::NotApplicable,
        );
        entry.planned_steps.push(OperationJournalStep::new(
            "refresh_remote_rules",
            OperationPhase::Running,
        ));

        store.create(entry);

        let mut completed =
            OperationJournalStep::new("refresh_remote_rules", OperationPhase::Running);
        completed.result = crate::state::contracts::OperationStepResult::Completed;
        let mut progress =
            OperationJournalStep::new("refresh_remote_rules_progress", OperationPhase::Running);
        progress.result = crate::state::contracts::OperationStepResult::Completed;
        store.record_progress(&operation_id, progress);
        store.record_success(&operation_id, completed, OperationOutcome::Succeeded);

        let stored = store.get(&operation_id).expect("journal record");
        assert_eq!(stored.status, OperationStatus::Succeeded);
        assert_eq!(stored.completed_steps.len(), 2);
        assert_eq!(
            stored.completed_steps[0].step_id,
            "refresh_remote_rules_progress"
        );
        assert_eq!(stored.outcome, Some(OperationOutcome::Succeeded));
        assert_eq!(
            store
                .latest_for_command(CommandKind::RefreshPerformanceRules)
                .expect("latest journal")
                .operation_id,
            operation_id
        );
    }
}
