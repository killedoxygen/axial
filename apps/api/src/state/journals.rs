use super::contracts::{
    CommandKind, OperationId, OperationJournalEntry, OperationJournalStep, OperationOutcome,
    OperationStatus, TargetDescriptor,
};
use super::ownership::{CurrentArtifact, classify_current_artifact};
use crate::execution::file::{FileWriteRequest, write_file_atomically};
use crate::observability::{
    RedactionAudience, evidence_text_looks_sensitive, sanitize_evidence_text,
};
use croopor_config::AppPaths;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::RwLock;
use tracing::warn;

pub const OPERATION_JOURNAL_SCHEMA: &str = "croopor.state.operation_journals.v1";
pub const DEFAULT_OPERATION_JOURNAL_LIMIT: usize = 128;
const OPERATION_JOURNAL_FILE: &str = "operation-journals.json";

pub struct OperationJournalStore {
    records: RwLock<BTreeMap<String, OperationJournalEntry>>,
    max_entries: usize,
    storage_path: Option<PathBuf>,
}

impl OperationJournalStore {
    pub fn new() -> Self {
        Self::with_max_entries(DEFAULT_OPERATION_JOURNAL_LIMIT)
    }

    pub fn with_max_entries(max_entries: usize) -> Self {
        Self {
            records: RwLock::new(BTreeMap::new()),
            max_entries: max_entries.max(1),
            storage_path: None,
        }
    }

    pub fn load_from_paths(paths: &AppPaths) -> Self {
        let storage_path = operation_journal_path(paths);
        let store = Self::with_max_entries_and_storage(
            DEFAULT_OPERATION_JOURNAL_LIMIT,
            Some(storage_path.clone()),
        );

        match fs::read_to_string(&storage_path) {
            Ok(data) => match OperationJournalSnapshot::from_json(&data) {
                Ok(snapshot) => {
                    if let Err(error) = store.load_snapshot(snapshot) {
                        warn!(
                            error = ?error,
                            "failed to load persisted operation journal snapshot"
                        );
                    }
                }
                Err(error) => {
                    warn!(
                        error = ?error,
                        "failed to parse persisted operation journal snapshot"
                    );
                }
            },
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                warn!(
                    error = %error,
                    "failed to read persisted operation journal snapshot"
                );
            }
        }

        store
    }

    fn with_max_entries_and_storage(max_entries: usize, storage_path: Option<PathBuf>) -> Self {
        Self {
            records: RwLock::new(BTreeMap::new()),
            max_entries: max_entries.max(1),
            storage_path,
        }
    }

    pub fn create(&self, entry: OperationJournalEntry) {
        if let Err(error) = validate_entry(&entry) {
            warn!(error = ?error, "refused invalid operation journal entry");
            return;
        }
        if let Ok(mut records) = self.records.write() {
            records.insert(entry.operation_id.as_str().to_string(), entry);
            prune_records(&mut records, self.max_entries);
        }
        self.persist_snapshot();
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
        let mut should_persist = false;
        if let Ok(mut records) = self.records.write()
            && let Some(entry) = records.get(operation_id.as_str()).cloned()
        {
            let mut candidate = entry;
            update(&mut candidate);
            if let Err(error) = validate_entry(&candidate) {
                warn!(error = ?error, "operation journal entry failed validation after update");
            } else {
                records.insert(operation_id.as_str().to_string(), candidate);
                prune_records(&mut records, self.max_entries);
                should_persist = true;
            }
        }
        if should_persist {
            self.persist_snapshot();
        }
    }

    pub fn list(&self) -> Vec<OperationJournalEntry> {
        self.records
            .read()
            .map(|records| records.values().cloned().collect())
            .unwrap_or_default()
    }

    pub fn snapshot(&self) -> Result<OperationJournalSnapshot, OperationJournalLoadError> {
        OperationJournalSnapshot::new(self.list())
    }

    pub fn load_snapshot(
        &self,
        snapshot: OperationJournalSnapshot,
    ) -> Result<(), OperationJournalLoadError> {
        snapshot.validate()?;
        if let Ok(mut records) = self.records.write() {
            records.clear();
            for entry in snapshot.entries {
                records.insert(entry.operation_id.as_str().to_string(), entry);
            }
            prune_records(&mut records, self.max_entries);
        }
        Ok(())
    }

    fn persist_snapshot(&self) {
        let Some(storage_path) = self.storage_path.as_deref() else {
            return;
        };
        let snapshot = match self.snapshot() {
            Ok(snapshot) => snapshot,
            Err(error) => {
                warn!(error = ?error, "failed to build operation journal snapshot");
                return;
            }
        };
        if let Err(error) = persist_snapshot_to_path(storage_path, &snapshot) {
            warn!(
                error = %error,
                "failed to persist operation journal snapshot"
            );
        }
    }
}

impl Default for OperationJournalStore {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OperationJournalSnapshot {
    pub schema: String,
    pub entries: Vec<OperationJournalEntry>,
}

impl OperationJournalSnapshot {
    pub fn new(entries: Vec<OperationJournalEntry>) -> Result<Self, OperationJournalLoadError> {
        let snapshot = Self {
            schema: OPERATION_JOURNAL_SCHEMA.to_string(),
            entries,
        };
        snapshot.validate()?;
        Ok(snapshot)
    }

    pub fn from_json(value: &str) -> Result<Self, OperationJournalLoadError> {
        let snapshot = serde_json::from_str::<Self>(value)?;
        snapshot.validate()?;
        Ok(snapshot)
    }

    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self)
    }

    fn validate(&self) -> Result<(), OperationJournalLoadError> {
        if self.schema != OPERATION_JOURNAL_SCHEMA {
            return Err(OperationJournalLoadError::InvalidSchema);
        }
        if self.entries.len() > DEFAULT_OPERATION_JOURNAL_LIMIT {
            return Err(OperationJournalLoadError::TooManyEntries);
        }
        for entry in &self.entries {
            validate_entry(entry)?;
        }
        Ok(())
    }
}

#[derive(Debug)]
pub enum OperationJournalLoadError {
    Json(serde_json::Error),
    InvalidSchema,
    TooManyEntries,
    InvalidEntry(OperationJournalValidationError),
}

impl From<serde_json::Error> for OperationJournalLoadError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error)
    }
}

impl From<OperationJournalValidationError> for OperationJournalLoadError {
    fn from(error: OperationJournalValidationError) -> Self {
        Self::InvalidEntry(error)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OperationJournalValidationError {
    UnsafeJournalId,
    UnsafeOperationId,
    UnsafeTargetId,
    UnsafeStepId,
    UnsafeGeneratedFact,
    UnsafeFailurePoint,
    UnsafeDiagnosisId,
    EmptyJournal,
    TooManyTargets,
    TooManyPlannedSteps,
    TooManyCompletedSteps,
    TooManyFacts,
    TooManyDiagnoses,
}

fn validate_entry(entry: &OperationJournalEntry) -> Result<(), OperationJournalValidationError> {
    if !safe_token(entry.journal_id.as_str(), 128) {
        return Err(OperationJournalValidationError::UnsafeJournalId);
    }
    if !safe_token(entry.operation_id.as_str(), 128) {
        return Err(OperationJournalValidationError::UnsafeOperationId);
    }
    if entry.targets.len() > 16 {
        return Err(OperationJournalValidationError::TooManyTargets);
    }
    for target in &entry.targets {
        validate_target(target)?;
    }
    if entry.planned_steps.len() > 128 {
        return Err(OperationJournalValidationError::TooManyPlannedSteps);
    }
    for step in &entry.planned_steps {
        validate_step(step)?;
    }
    if entry.completed_steps.len() > 256 {
        return Err(OperationJournalValidationError::TooManyCompletedSteps);
    }
    for step in &entry.completed_steps {
        validate_step(step)?;
    }
    if let Some(failure_point) = &entry.failure_point
        && !safe_token(failure_point, 96)
    {
        return Err(OperationJournalValidationError::UnsafeFailurePoint);
    }
    if entry.guardian_diagnosis_ids.len() > 32 {
        return Err(OperationJournalValidationError::TooManyDiagnoses);
    }
    for diagnosis_id in &entry.guardian_diagnosis_ids {
        if !safe_token(diagnosis_id, 96) {
            return Err(OperationJournalValidationError::UnsafeDiagnosisId);
        }
    }
    Ok(())
}

fn validate_target(target: &TargetDescriptor) -> Result<(), OperationJournalValidationError> {
    if !safe_token(&target.id, 96) {
        return Err(OperationJournalValidationError::UnsafeTargetId);
    }
    Ok(())
}

fn validate_step(step: &OperationJournalStep) -> Result<(), OperationJournalValidationError> {
    if !safe_token(&step.step_id, 96) {
        return Err(OperationJournalValidationError::UnsafeStepId);
    }
    if let Some(target) = &step.changed_target {
        validate_target(target)?;
    }
    if step.generated_facts.len() > 64 {
        return Err(OperationJournalValidationError::TooManyFacts);
    }
    for fact in &step.generated_facts {
        if !safe_public_fragment(fact, 320) {
            return Err(OperationJournalValidationError::UnsafeGeneratedFact);
        }
    }
    Ok(())
}

fn safe_token(value: &str, max_chars: usize) -> bool {
    let value = value.trim();
    !value.is_empty()
        && !value.chars().any(char::is_control)
        && value.chars().count() <= max_chars
        && value.chars().all(|value| {
            value.is_ascii_alphanumeric() || matches!(value, '-' | '_' | '.' | '+' | ':')
        })
        && !structured_token_looks_sensitive(value)
}

fn safe_public_fragment(value: &str, max_chars: usize) -> bool {
    let value = value.trim();
    !value.is_empty()
        && !value.chars().any(char::is_control)
        && !evidence_text_looks_sensitive(value)
        && sanitize_evidence_text(value, RedactionAudience::UserVisible, max_chars).is_some()
}

fn structured_token_looks_sensitive(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    if value.contains('/') || value.contains('\\') || contains_windows_drive_path(value) {
        return true;
    }
    if lower.contains(".jar")
        || lower.contains(".exe")
        || lower.contains(".dll")
        || lower.contains(".dylib")
        || lower.contains(".so")
        || lower.contains("-xmx")
        || lower.contains("-xms")
        || lower.contains("-xx:")
        || lower.starts_with("-d")
        || lower.contains("--")
        || lower.contains("token")
        || lower.contains("secret")
        || lower.contains("password")
        || lower.contains("provider_payload")
        || lower.contains("account_id")
        || lower.contains("username=")
        || lower.contains("xuid=")
        || lower.contains("authorization")
        || lower.contains("credential")
        || lower.contains("bearer")
    {
        return true;
    }
    if value.contains('@') && value.contains('.') {
        return true;
    }
    looks_like_jwt_token(value) || has_long_secret_like_segment(value)
}

fn contains_windows_drive_path(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.windows(3).any(|window| {
        window[0].is_ascii_alphabetic() && window[1] == b':' && matches!(window[2], b'\\' | b'/')
    })
}

fn looks_like_jwt_token(value: &str) -> bool {
    let parts = value.split('.').collect::<Vec<_>>();
    parts.len() >= 3
        && parts.iter().take(3).all(|part| {
            part.len() >= 12
                && part
                    .chars()
                    .all(|value| value.is_ascii_alphanumeric() || matches!(value, '-' | '_'))
        })
}

fn has_long_secret_like_segment(value: &str) -> bool {
    value
        .split(|value: char| !value.is_ascii_alphanumeric())
        .any(|part| {
            part.len() >= 48
                && part.chars().any(|value| value.is_ascii_alphabetic())
                && part.chars().any(|value| value.is_ascii_digit())
        })
}

fn prune_records(records: &mut BTreeMap<String, OperationJournalEntry>, max_entries: usize) {
    if records.len() <= max_entries {
        return;
    }
    let remove_count = records.len().saturating_sub(max_entries);
    let keys = records
        .keys()
        .take(remove_count)
        .cloned()
        .collect::<Vec<_>>();
    for key in keys {
        records.remove(&key);
    }
}

pub fn operation_journal_path(paths: &AppPaths) -> PathBuf {
    paths.config_dir.join("state").join(OPERATION_JOURNAL_FILE)
}

fn persist_snapshot_to_path(path: &Path, snapshot: &OperationJournalSnapshot) -> io::Result<()> {
    let data = snapshot
        .to_json()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    write_file_atomically(FileWriteRequest::new(
        classify_current_artifact(
            CurrentArtifact::OperationJournalSnapshot,
            "operation_journal",
        )
        .target,
        path,
        data.as_bytes(),
    ))
    .map(|_| ())
    .map_err(io::Error::from)
}

#[cfg(test)]
mod tests {
    use super::{OperationJournalSnapshot, OperationJournalStore, operation_journal_path};
    use crate::state::contracts::{
        CommandKind, JournalId, OperationId, OperationJournalEntry, OperationJournalStep,
        OperationOutcome, OperationPhase, OperationStatus, OwnershipClass, RollbackState,
        StabilizationSystem, TargetDescriptor, TargetKind,
    };
    use croopor_config::AppPaths;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

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

    #[test]
    fn operation_journal_snapshot_round_trips_strict_shape() {
        let entry = test_entry("operation-1");
        let snapshot = OperationJournalSnapshot::new(vec![entry.clone()]).expect("snapshot");
        let encoded = snapshot.to_json().expect("serialize snapshot");
        let decoded = OperationJournalSnapshot::from_json(&encoded).expect("deserialize snapshot");

        assert_eq!(decoded.entries, vec![entry]);

        let unknown_field = serde_json::json!({
            "schema": super::OPERATION_JOURNAL_SCHEMA,
            "entries": [{
                "journal_id": "journal-operation-1",
                "operation_id": "operation-1",
                "command": "InstallVersion",
                "status": "Succeeded",
                "owner": "Application",
                "ownership": "LauncherManaged",
                "targets": [],
                "planned_steps": [],
                "completed_steps": [],
                "failure_point": null,
                "rollback": "NotApplicable",
                "guardian_diagnosis_ids": [],
                "outcome": "Succeeded",
                "unexpected": true
            }]
        });
        assert!(OperationJournalSnapshot::from_json(&unknown_field.to_string()).is_err());
    }

    #[test]
    fn operation_journal_snapshot_rejects_raw_public_evidence() {
        let mut entry = test_entry("operation-raw");
        entry.completed_steps[0]
            .generated_facts
            .push(r"C:\Users\Alice\.minecraft --accessToken secret -Xmx8192M".to_string());

        assert!(OperationJournalSnapshot::new(vec![entry]).is_err());

        let mut unsafe_target = test_entry("operation-unsafe-target");
        unsafe_target.targets.push(TargetDescriptor {
            system: StabilizationSystem::State,
            kind: TargetKind::FilesystemPath,
            id: "/home/alice/.croopor/libraries/secret.jar".to_string(),
            ownership: OwnershipClass::LauncherManaged,
        });
        assert!(OperationJournalSnapshot::new(vec![unsafe_target]).is_err());
    }

    #[test]
    fn structured_tokens_accept_uuid_ids_without_allowing_secret_runs() {
        assert!(super::safe_token(
            "performance-rules-refresh-123e4567-e89b-12d3-a456-426614174000",
            128,
        ));
        assert!(super::safe_token(
            "guardian-artifact-repair:123e4567-e89b-12d3-a456-426614174000",
            128,
        ));
        assert!(!super::safe_token(
            "operation-abcdefghijklmnopqrstuvwxyzabcdefghijklmnopqrstuvwxyz123456",
            128,
        ));
        assert!(!super::safe_token("operation-access-token-secret", 128));
    }

    #[test]
    fn journal_store_persists_snapshot_for_restart_replay() {
        let root = test_root("persisted-journal");
        let paths = test_paths(&root);
        let store = OperationJournalStore::load_from_paths(&paths);
        let operation_id = OperationId::new("install-operation-restart-replay");
        let mut entry = test_entry(operation_id.as_str());
        entry.operation_id = operation_id.clone();
        entry.journal_id = JournalId::new(format!("journal-{}", operation_id.as_str()));

        store.create(entry);
        store.record_guardian_evidence(
            &operation_id,
            vec![
                "guardian_outcome_decision:retry".to_string(),
                "guardian_outcome_summary:Guardian treated install download failure as retryable."
                    .to_string(),
            ],
            vec!["download_unavailable".to_string()],
        );

        let path = operation_journal_path(&paths);
        assert!(path.is_file());
        let snapshot = OperationJournalSnapshot::from_json(
            &fs::read_to_string(&path).expect("persisted journal snapshot"),
        )
        .expect("valid persisted snapshot");
        assert_eq!(snapshot.entries.len(), 1);

        let reloaded = OperationJournalStore::load_from_paths(&paths);
        let loaded = reloaded.get(&operation_id).expect("reloaded journal");
        assert_eq!(loaded.operation_id, operation_id);
        assert_eq!(loaded.status, OperationStatus::Succeeded);
        assert!(
            loaded
                .guardian_diagnosis_ids
                .iter()
                .any(|id| id == "download_unavailable")
        );

        cleanup(&root);
    }

    #[test]
    fn journal_store_bounds_retention_to_current_entries() {
        let store = OperationJournalStore::with_max_entries(2);
        for id in ["operation-1", "operation-2", "operation-3"] {
            store.create(test_entry(id));
        }

        let entries = store.list();
        assert_eq!(entries.len(), 2);
        assert!(
            entries
                .iter()
                .all(|entry| entry.operation_id.as_str() != "operation-1")
        );
    }

    #[test]
    fn journal_store_rejects_invalid_update_without_mutating_record() {
        let store = OperationJournalStore::new();
        let operation_id = OperationId::new("operation-invalid-update");
        store.create(test_entry(operation_id.as_str()));

        store.record_guardian_evidence(
            &operation_id,
            vec![r"C:\Users\Alice\.minecraft --accessToken secret -Xmx8192M".to_string()],
            Vec::new(),
        );

        let entry = store.get(&operation_id).expect("journal");
        let facts = &entry.completed_steps[0].generated_facts;
        assert!(!facts.iter().any(|fact| fact.contains("accessToken")));
        assert_eq!(facts, &vec!["install_phase:done", "install_done:true"]);
    }

    fn test_entry(operation_id: &str) -> OperationJournalEntry {
        let operation_id = OperationId::new(operation_id);
        let mut entry = OperationJournalEntry::new(
            JournalId::new(format!("journal-{}", operation_id.as_str())),
            operation_id,
            CommandKind::InstallVersion,
            StabilizationSystem::Application,
            OwnershipClass::LauncherManaged,
            RollbackState::NotApplicable,
        );
        entry.status = OperationStatus::Succeeded;
        entry.targets.push(TargetDescriptor::new(
            StabilizationSystem::Application,
            TargetKind::Version,
            "minecraft_1.21.5",
            OwnershipClass::LauncherManaged,
        ));
        let mut completed =
            OperationJournalStep::new("install_progress_done", OperationPhase::Completed);
        completed.result = crate::state::contracts::OperationStepResult::Completed;
        completed
            .generated_facts
            .push("install_phase:done".to_string());
        completed
            .generated_facts
            .push("install_done:true".to_string());
        entry.completed_steps.push(completed);
        entry.outcome = Some(OperationOutcome::Succeeded);
        entry
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

    fn test_root(prefix: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|value| value.as_nanos())
            .unwrap_or_default();
        std::env::temp_dir().join(format!(
            "croopor-operation-journal-{prefix}-{}-{nanos:x}",
            std::process::id()
        ))
    }

    fn cleanup(root: &Path) {
        let _ = fs::remove_dir_all(root);
    }
}
