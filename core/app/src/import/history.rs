//! Bounded predecessor terminal evidence, never session or process authority.

use super::{ImportBlocker, ImportError, ImportResult, Inventory};
use crate::{
    install::history::{
        PreparedImport as PreparedInstallImport, PreparedOperation as PreparedInstallOperation,
        SourceGuardianTerminal, SourceIntent, SourceMetrics, SourceOperation, SourceStep,
        SourceTarget as LegacyPerformanceTarget,
    },
    instances::model::InstanceId,
    launch::{
        logs::Redactor,
        outcome::{FailureClass, SessionExitReason, SessionOutcome, SessionOutcomeKind},
        reports::{
            CrashEvidence, LaunchProofComparison, LaunchProofDevice, LaunchProofRecord,
            LaunchProofResourceBudget, LaunchProofScenario, LaunchProofStage,
            LaunchProofStageEvidence, MAX_REPORT_BYTES, PreparedReportImport,
        },
    },
    performance::{
        benchmarks::{
            BenchmarkSuiteDriverStatus, BenchmarkSuiteManifest, BenchmarkSuiteManifestRun,
            PreparedBenchmarkImport,
        },
        mutation::{
            HistoricalOperation, HistoricalOperationEvidence, HistoricalOperationIntent,
            HistoricalOperationTerminal, HistoricalRollback, PreparedOperationImport,
        },
        rules::{
            CompletedRulesImport, HistoricalRulesRefresh, HistoricalRulesRefreshFailure,
            HistoricalRulesRefreshOutcome,
        },
    },
};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

const REPORT_PREFIX: &str = "profile/benchmarks/launch/";
const SUITE_PREFIX: &str = "profile/benchmarks/suites/";
const DRIVER_PREFIX: &str = "profile/benchmarks/suite-drivers/";
const JOURNAL: &str = "profile/state/operation-journals.json";
const MAX_HISTORY_RECORDS: usize = 1024;
const MAX_HISTORY_BYTES: usize = 64 * 1024 * 1024;
const MAX_BENCHMARK_BYTES: usize = 256 * 1024;
const UNBOUND_INSTANCE: &str = "00000000-0000-0000-0000-000000000001";

#[derive(Clone)]
pub(crate) struct PreparedHistory {
    legacy_id: String,
    records: Vec<LaunchProofRecord>,
    suites: Vec<BenchmarkSuiteManifest>,
    drivers: Vec<BenchmarkSuiteDriverStatus>,
    operations: Vec<HistoricalOperation>,
    global_installs: Arc<[PreparedInstallOperation]>,
    archived_reports: Arc<PreparedReportImport>,
    archived_benchmarks: Arc<PreparedBenchmarkImport>,
    content: Vec<PreparedInstallOperation>,
    rules: Option<CompletedRulesImport>,
}

pub(crate) struct BoundHistory {
    pub(crate) reports: PreparedReportImport,
    pub(crate) benchmarks: PreparedBenchmarkImport,
    pub(crate) operations: PreparedOperationImport,
    pub(crate) installs: PreparedInstallImport,
    pub(crate) rules: Option<CompletedRulesImport>,
}

/// One immutable conversion of all retained reports, shared by preview rows.
pub(super) struct PreparedSourceHistory {
    instances: BTreeMap<String, PreparedHistory>,
    supported_records: BTreeSet<String>,
    supported_blockers: BTreeSet<ImportBlocker>,
}

impl PreparedSourceHistory {
    #[cfg(test)]
    pub(super) fn supported_records(&self) -> &BTreeSet<String> {
        &self.supported_records
    }

    pub(super) fn supports(&self, blocker: &ImportBlocker) -> bool {
        self.supported_blockers.contains(blocker)
    }

    pub(super) fn for_instance(&self, legacy_id: &str) -> ImportResult<PreparedHistory> {
        self.instances
            .get(legacy_id)
            .cloned()
            .ok_or(ImportError::InvalidData)
    }
}

impl PreparedHistory {
    /// A retry's reserved UUID can differ from the freshly prepared candidate.
    /// Bind and serialize before entering the final publication transaction.
    pub(crate) fn bind_instance(&self, instance: &InstanceId) -> ImportResult<BoundHistory> {
        let mut reports = self.records.clone();
        for report in &mut reports {
            report.instance_id = instance.as_str().to_owned();
        }
        let mut suites = self.suites.clone();
        for suite in &mut suites {
            suite.instance_id = instance.as_str().to_owned();
        }
        let mut operations = self.operations.clone();
        for operation in &mut operations {
            operation.instance_id = instance.clone();
        }
        let mut reports =
            PreparedReportImport::prepare(reports).map_err(|_| ImportError::InvalidData)?;
        reports
            .append(&self.archived_reports)
            .map_err(|_| ImportError::InvalidData)?;
        let mut benchmarks = PreparedBenchmarkImport::prepare(suites, self.drivers.clone())
            .map_err(|_| ImportError::InvalidData)?;
        benchmarks
            .append(&self.archived_benchmarks)
            .map_err(|_| ImportError::InvalidData)?;
        Ok(BoundHistory {
            reports,
            benchmarks,
            operations: PreparedOperationImport::prepare(operations)
                .map_err(|_| ImportError::InvalidData)?,
            installs: PreparedInstallImport::bind(
                self.global_installs
                    .iter()
                    .chain(&self.content)
                    .cloned()
                    .collect(),
                &self.legacy_id,
                instance,
            )
            .map_err(|_| ImportError::InvalidData)?,
            rules: self.rules.clone(),
        })
    }
}

pub(super) fn prepare_history(inventory: &Inventory) -> ImportResult<PreparedSourceHistory> {
    prepare_history_with_rules(inventory, None)
}

pub(super) fn prepare_history_with_rules(
    inventory: &Inventory,
    rules: Option<&CompletedRulesImport>,
) -> ImportResult<PreparedSourceHistory> {
    if let Some(rules) = rules {
        inventory.validate_rules_completion(rules)?;
    }
    let source = inventory.source_identity()?;
    source_instance_ids(inventory)?;
    let empty_archived = Arc::new(
        PreparedReportImport::prepare_archived(&source, Vec::new())
            .map_err(|_| ImportError::InvalidData)?,
    );
    let empty_benchmarks = Arc::new(
        PreparedBenchmarkImport::prepare_archived(&source, Vec::new(), Vec::new())
            .map_err(|_| ImportError::InvalidData)?,
    );
    let mut prepared = PreparedSourceHistory {
        instances: inventory
            .instances()
            .iter()
            .map(|instance| {
                (
                    instance.legacy_id.clone(),
                    PreparedHistory {
                        legacy_id: instance.legacy_id.clone(),
                        records: Vec::new(),
                        suites: Vec::new(),
                        drivers: Vec::new(),
                        operations: Vec::new(),
                        global_installs: Arc::from([]),
                        archived_reports: Arc::clone(&empty_archived),
                        archived_benchmarks: Arc::clone(&empty_benchmarks),
                        content: Vec::new(),
                        rules: rules.cloned(),
                    },
                )
            })
            .collect(),
        supported_records: BTreeSet::new(),
        supported_blockers: BTreeSet::from([
            ImportBlocker::RetainedHistoryRequiresConversion,
            ImportBlocker::UnsettledOperation,
        ]),
    };
    let mut bytes = 0usize;
    let mut global_installs = Vec::new();
    if inventory
        .file_manifests()
        .any(|file| file.relative == JOURNAL)
    {
        let raw = inventory.record_bytes(JOURNAL)?;
        if raw.len() > 8 * 1024 * 1024 {
            return Err(ImportError::LimitExceeded);
        }
        bytes += raw.len();
        for (index, entry) in decode_journal(&raw)?.into_iter().enumerate() {
            let instance = match &entry.intent {
                LegacyIntent::Performance(intent) => intent.intent.instance_id.clone(),
                LegacyIntent::Generic {} => {
                    match entry.command.as_str() {
                        "RefreshPerformanceRules" => {
                            entry.convert_rules()?;
                            if rules.is_none() {
                                continue;
                            }
                        }
                        "InstallVersion" | "ModifyInstanceContent" => {
                            let operation = entry.convert_install(&source)?;
                            if let Some(instance) = operation.legacy_instance_id() {
                                prepared
                                    .instances
                                    .get_mut(instance)
                                    .ok_or(ImportError::InvalidData)?
                                    .content
                                    .push(operation);
                            } else {
                                global_installs.push(operation);
                            }
                        }
                        _ => return Err(ImportError::InvalidData),
                    }
                    prepared
                        .supported_records
                        .insert(format!("{JOURNAL}#/entries/{index}"));
                    continue;
                }
            };
            let operation = entry.convert(&source)?;
            prepared
                .instances
                .get_mut(&instance)
                .ok_or(ImportError::InvalidData)?
                .operations
                .push(operation);
            prepared
                .supported_records
                .insert(format!("{JOURNAL}#/entries/{index}"));
        }
    }
    PreparedInstallImport::validate(
        &global_installs
            .iter()
            .chain(
                prepared
                    .instances
                    .values()
                    .flat_map(|history| &history.content),
            )
            .cloned()
            .collect::<Vec<_>>(),
    )
    .map_err(|_| ImportError::InvalidData)?;
    let global_installs: Arc<[PreparedInstallOperation]> = global_installs.into();
    for history in prepared.instances.values_mut() {
        history.global_installs = global_installs.clone();
    }
    let mut reports = BTreeMap::new();
    let mut archived_reports = Vec::new();
    let mut suites = BTreeMap::new();
    let mut drivers = Vec::new();
    for obligation in inventory
        .obligations()
        .iter()
        .filter(|item| item.blocker == ImportBlocker::RetainedHistoryRequiresConversion)
    {
        if prepared.supported_records.len() == MAX_HISTORY_RECORDS {
            return Err(ImportError::LimitExceeded);
        }
        let path = obligation.source_record.as_str();
        let raw = inventory.record_bytes(&obligation.source_record)?;
        bytes = bytes
            .checked_add(raw.len())
            .ok_or(ImportError::LimitExceeded)?;
        if bytes > MAX_HISTORY_BYTES {
            return Err(ImportError::LimitExceeded);
        }
        if let Some(filename) = path.strip_prefix(REPORT_PREFIX) {
            if raw.len() > MAX_REPORT_BYTES {
                return Err(ImportError::LimitExceeded);
            }
            #[cfg(test)]
            tests::record_preparation();
            let legacy: LegacyReport =
                serde_json::from_slice(&raw).map_err(|_| ImportError::InvalidData)?;
            let legacy_instance = legacy.instance_id.clone();
            let session = legacy.session_id.clone();
            let proof = legacy.source_proof();
            let report = legacy.convert(&source, filename)?;
            if reports.insert(session, proof).is_some() {
                return Err(ImportError::InvalidData);
            }
            if let Some(selected) = prepared.instances.get_mut(&legacy_instance) {
                PreparedReportImport::prepare(vec![report.clone()])
                    .map_err(|_| ImportError::InvalidData)?;
                selected.records.push(report);
            } else {
                archived_reports.push((legacy_instance, report));
            }
        } else if let Some(filename) = path.strip_prefix(SUITE_PREFIX) {
            if raw.len() > MAX_BENCHMARK_BYTES {
                return Err(ImportError::LimitExceeded);
            }
            let suite: LegacySuite =
                serde_json::from_slice(&raw).map_err(|_| ImportError::InvalidData)?;
            if filename != format!("{}.json", suite.suite_id)
                || suites.insert(suite.suite_id.clone(), suite).is_some()
            {
                return Err(ImportError::InvalidData);
            }
        } else if let Some(filename) = path.strip_prefix(DRIVER_PREFIX) {
            if raw.len() > MAX_BENCHMARK_BYTES {
                return Err(ImportError::LimitExceeded);
            }
            let driver: LegacyDriver =
                serde_json::from_slice(&raw).map_err(|_| ImportError::InvalidData)?;
            if filename != format!("{}.json", driver.id) {
                return Err(ImportError::InvalidData);
            }
            drivers.push(driver);
        } else {
            return Err(ImportError::InvalidData);
        }
        prepared
            .supported_records
            .insert(obligation.source_record.clone());
    }
    let mut claimed_sessions = BTreeSet::new();
    let mut archived_suites = Vec::new();
    let mut archived_drivers = Vec::new();
    for suite in suites.values() {
        let converted = suite.convert(&source, &reports, &mut claimed_sessions)?;
        if let Some(selected) = prepared.instances.get_mut(&suite.instance_id) {
            selected.suites.push(converted);
        } else {
            archived_suites.push((suite.instance_id.clone(), converted));
        }
    }
    let mut driver_ids = BTreeSet::new();
    for driver in drivers {
        if !driver_ids.insert(driver.id.clone()) {
            return Err(ImportError::InvalidData);
        }
        let suite = suites
            .get(&driver.suite_id)
            .ok_or(ImportError::InvalidData)?;
        let converted = driver.convert(&source, suite, &reports)?;
        if let Some(selected) = prepared.instances.get_mut(&suite.instance_id) {
            selected.drivers.push(converted);
        } else {
            archived_drivers.push(converted);
        }
    }
    let archived_reports = Arc::new(
        PreparedReportImport::prepare_archived(&source, archived_reports)
            .map_err(|_| ImportError::InvalidData)?,
    );
    let archived_benchmarks = Arc::new(
        PreparedBenchmarkImport::prepare_archived(&source, archived_suites, archived_drivers)
            .map_err(|_| ImportError::InvalidData)?,
    );
    for history in prepared.instances.values_mut() {
        history.archived_reports = Arc::clone(&archived_reports);
        history.archived_benchmarks = Arc::clone(&archived_benchmarks);
        PreparedBenchmarkImport::prepare(history.suites.clone(), history.drivers.clone())
            .map_err(|_| ImportError::InvalidData)?;
        PreparedOperationImport::prepare(history.operations.clone())
            .map_err(|_| ImportError::InvalidData)?;
    }
    for obligation in inventory.obligations() {
        if !prepared
            .supported_records
            .contains(&obligation.source_record)
        {
            prepared.supported_blockers.remove(&obligation.blocker);
        }
    }
    Ok(prepared)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SourceRegistry {
    schema_version: u32,
    #[serde(rename = "last_instance_id")]
    _last_instance_id: String,
    pending_deletions: Vec<serde::de::IgnoredAny>,
    instances: Vec<SourceInstanceIdentity>,
}

struct SourceInstanceIdentity(String);

impl<'de> Deserialize<'de> for SourceInstanceIdentity {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct IdentityVisitor;
        impl<'de> serde::de::Visitor<'de> for IdentityVisitor {
            type Value = SourceInstanceIdentity;
            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a recorded instance with a unique identity field")
            }
            fn visit_map<M: serde::de::MapAccess<'de>>(
                self,
                mut map: M,
            ) -> Result<Self::Value, M::Error> {
                let mut fields = BTreeSet::new();
                let mut id = None;
                while let Some(key) = map.next_key::<String>()? {
                    if !fields.insert(key.clone()) {
                        return Err(serde::de::Error::custom("duplicate instance field"));
                    }
                    if key == "id" {
                        id = Some(map.next_value::<String>()?);
                    } else {
                        map.next_value::<serde::de::IgnoredAny>()?;
                    }
                }
                Ok(SourceInstanceIdentity(
                    id.ok_or_else(|| serde::de::Error::missing_field("id"))?,
                ))
            }
        }
        deserializer.deserialize_map(IdentityVisitor)
    }
}

fn source_instance_ids(inventory: &Inventory) -> ImportResult<BTreeSet<String>> {
    // Original v3 registry limits; capture itself admits a broader record set.
    if inventory
        .file_manifests()
        .find(|file| file.relative == "profile/instances.json")
        .is_none_or(|file| file.size > 1024 * 1024)
    {
        return Err(ImportError::InvalidData);
    }
    let registry: SourceRegistry =
        serde_json::from_slice(&inventory.record_bytes("profile/instances.json")?)
            .map_err(|_| ImportError::InvalidData)?;
    if registry.schema_version != 3
        || registry
            .instances
            .len()
            .saturating_add(registry.pending_deletions.len())
            > 1024
        || registry.pending_deletions.len() > 1
    {
        return Err(ImportError::InvalidData);
    }
    let mut ids = BTreeSet::new();
    for SourceInstanceIdentity(id) in registry.instances {
        if !super::model::legacy_id(&id) || !ids.insert(id) {
            return Err(ImportError::InvalidData);
        }
    }
    if ids
        != inventory
            .instances()
            .iter()
            .map(|instance| instance.legacy_id.clone())
            .collect()
    {
        return Err(ImportError::InvalidData);
    }
    Ok(ids)
}

pub(super) struct PreparedArchivedHistory {
    pub(super) reports: Arc<PreparedReportImport>,
    pub(super) benchmarks: Option<Arc<PreparedBenchmarkImport>>,
}

/// Metadata preserves supported archives without waiving unrelated obligations.
/// Benchmark proof depends on this exact report batch, never on an inferred row.
pub(super) fn prepare_archived_history(
    inventory: &Inventory,
) -> ImportResult<PreparedArchivedHistory> {
    let instances = source_instance_ids(inventory)?;
    let source = inventory.source_identity()?;
    let mut archived = Vec::new();
    let mut proofs = BTreeMap::new();
    let mut bytes = 0usize;
    let mut count = 0usize;
    for file in inventory.file_manifests() {
        let Some(filename) = file.relative.strip_prefix(REPORT_PREFIX) else {
            continue;
        };
        count += 1;
        if count > MAX_HISTORY_RECORDS || file.size > MAX_REPORT_BYTES as u64 {
            return Err(ImportError::LimitExceeded);
        }
        let raw = inventory.record_bytes(&file.relative)?;
        bytes = bytes
            .checked_add(raw.len())
            .ok_or(ImportError::LimitExceeded)?;
        if bytes > MAX_HISTORY_BYTES {
            return Err(ImportError::LimitExceeded);
        }
        let report: LegacyReport =
            serde_json::from_slice(&raw).map_err(|_| ImportError::InvalidData)?;
        if !instances.contains(&report.instance_id) {
            let instance = report.instance_id.clone();
            if proofs
                .insert(report.session_id.clone(), report.source_proof())
                .is_some()
            {
                return Err(ImportError::InvalidData);
            }
            archived.push((instance, report.convert(&source, filename)?));
        }
    }
    let reports = Arc::new(
        PreparedReportImport::prepare_archived(&source, archived)
            .map_err(|_| ImportError::InvalidData)?,
    );
    let benchmarks =
        match prepare_archived_benchmarks(inventory, &source, &instances, &proofs, count, bytes) {
            Ok(batch) => Some(Arc::new(batch)),
            Err(ImportError::InvalidData | ImportError::LimitExceeded) => None,
            Err(error) => return Err(error),
        };
    Ok(PreparedArchivedHistory {
        reports,
        benchmarks,
    })
}

fn prepare_archived_benchmarks(
    inventory: &Inventory,
    source: &str,
    instances: &BTreeSet<String>,
    reports: &BTreeMap<String, SourceReport>,
    mut count: usize,
    mut bytes: usize,
) -> ImportResult<PreparedBenchmarkImport> {
    let mut suites = BTreeMap::new();
    let mut drivers = Vec::new();
    for file in inventory.file_manifests().filter(|file| {
        file.relative.starts_with(SUITE_PREFIX) || file.relative.starts_with(DRIVER_PREFIX)
    }) {
        count += 1;
        if count > MAX_HISTORY_RECORDS || file.size > MAX_BENCHMARK_BYTES as u64 {
            return Err(ImportError::LimitExceeded);
        }
        let raw = inventory.record_bytes(&file.relative)?;
        bytes = bytes
            .checked_add(raw.len())
            .ok_or(ImportError::LimitExceeded)?;
        if bytes > MAX_HISTORY_BYTES {
            return Err(ImportError::LimitExceeded);
        }
        if let Some(filename) = file.relative.strip_prefix(SUITE_PREFIX) {
            let suite: LegacySuite =
                serde_json::from_slice(&raw).map_err(|_| ImportError::InvalidData)?;
            if filename != format!("{}.json", suite.suite_id)
                || suites.insert(suite.suite_id.clone(), suite).is_some()
            {
                return Err(ImportError::InvalidData);
            }
        } else {
            let driver: LegacyDriver =
                serde_json::from_slice(&raw).map_err(|_| ImportError::InvalidData)?;
            if file.relative != format!("{DRIVER_PREFIX}{}.json", driver.id) {
                return Err(ImportError::InvalidData);
            }
            drivers.push(driver);
        }
    }
    let mut archived_suites = Vec::new();
    let mut claimed_sessions = BTreeSet::new();
    for suite in suites
        .values()
        .filter(|suite| !instances.contains(&suite.instance_id))
    {
        archived_suites.push((
            suite.instance_id.clone(),
            suite.convert(source, reports, &mut claimed_sessions)?,
        ));
    }
    let mut archived_drivers = Vec::new();
    let mut driver_ids = BTreeSet::new();
    for driver in drivers {
        if !driver_ids.insert(driver.id.clone()) {
            return Err(ImportError::InvalidData);
        }
        // A pruned parent cannot identify the driver's original instance.
        let suite = suites
            .get(&driver.suite_id)
            .ok_or(ImportError::InvalidData)?;
        if !instances.contains(&suite.instance_id) {
            archived_drivers.push(driver.convert(source, suite, reports)?);
        }
    }
    PreparedBenchmarkImport::prepare_archived(source, archived_suites, archived_drivers)
        .map_err(|_| ImportError::InvalidData)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyJournal {
    schema: String,
    next_sequence: u64,
    entries: Vec<LegacyOperation>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyOperation {
    journal_id: String,
    operation_id: String,
    sequence: u64,
    parent_operation_id: Option<String>,
    command: String,
    intent: LegacyIntent,
    status: String,
    owner: String,
    ownership: String,
    targets: Vec<LegacyPerformanceTarget>,
    planned_steps: Vec<LegacyGuardianStep>,
    completed_steps: Vec<LegacyGuardianStep>,
    failure_point: Option<String>,
    rollback: HistoricalRollback,
    guardian_diagnosis_ids: Vec<String>,
    outcome: Option<String>,
    reconciliation_attempt: Option<serde_json::Value>,
    reconciliation_terminal: Option<serde_json::Value>,
    persisted_state_repair_attempt: Option<serde_json::Value>,
    persisted_state_repair_terminal: Option<serde_json::Value>,
    guardian_install_terminal: Option<SourceGuardianTerminal>,
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum LegacyIntent {
    Generic {},
    Performance(LegacyPerformanceLifecycle),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyPerformanceLifecycle {
    intent: HistoricalOperationIntent,
    phase: LegacyPerformanceTerminal,
    created_at: String,
    updated_at: String,
}

#[derive(Deserialize)]
#[serde(tag = "phase", rename_all = "snake_case", deny_unknown_fields)]
enum LegacyPerformanceTerminal {
    Terminal {
        terminal: HistoricalOperationTerminal,
    },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyGuardianStep {
    step_id: String,
    phase: String,
    result: String,
    changed_target: Option<LegacyPerformanceTarget>,
    generated_facts: Vec<String>,
    rollback: HistoricalRollback,
    guardian_fact_ids: Vec<String>,
    metrics: Option<SourceMetrics>,
}

impl LegacyGuardianStep {
    fn into_install(self) -> SourceStep {
        SourceStep {
            step_id: self.step_id,
            phase: self.phase,
            result: self.result,
            changed_target: self.changed_target,
            generated_facts: self.generated_facts,
            rollback: rollback_label(self.rollback).to_owned(),
            guardian_fact_ids: self.guardian_fact_ids,
            metrics: self.metrics,
        }
    }
}

fn rollback_label(rollback: HistoricalRollback) -> &'static str {
    match rollback {
        HistoricalRollback::NotApplicable => "NotApplicable",
        HistoricalRollback::Available => "Available",
        HistoricalRollback::Unavailable => "Unavailable",
        HistoricalRollback::Applied => "Applied",
    }
}

fn decode_journal(raw: &[u8]) -> ImportResult<Vec<LegacyOperation>> {
    if raw.len() > 8 * 1024 * 1024 {
        return Err(ImportError::LimitExceeded);
    }
    let journal: LegacyJournal =
        serde_json::from_slice(raw).map_err(|_| ImportError::InvalidData)?;
    if journal.schema != "axial.state.operation_journals.v10"
        || journal.entries.len() > 128
        || journal.next_sequence == 0
    {
        return Err(ImportError::InvalidData);
    }
    let mut ids = BTreeSet::new();
    let mut sequences = BTreeSet::new();
    for entry in &journal.entries {
        if !ids.insert(&entry.operation_id)
            || entry.sequence == 0
            || !sequences.insert(entry.sequence)
            || entry.sequence >= journal.next_sequence
        {
            return Err(ImportError::InvalidData);
        }
    }
    Ok(journal.entries)
}

pub(super) fn prepare_global_install_history(
    inventory: &Inventory,
) -> ImportResult<PreparedInstallImport> {
    let mut records = Vec::new();
    if inventory
        .file_manifests()
        .any(|file| file.relative == JOURNAL)
    {
        let source = inventory.source_identity()?;
        // Decode the original whole journal before selecting this domain. An
        // unsupported or malformed global record must not become an empty proof.
        for entry in decode_journal(&inventory.record_bytes(JOURNAL)?)? {
            if entry.command == "InstallVersion" {
                records.push(entry.convert_install(&source)?);
            }
        }
    }
    PreparedInstallImport::bind_global(records).map_err(|_| ImportError::InvalidData)
}

pub(super) fn prepare_rules_history(
    inventory: &Inventory,
) -> ImportResult<Vec<HistoricalRulesRefresh>> {
    if !inventory
        .file_manifests()
        .any(|file| file.relative == JOURNAL)
    {
        return Ok(Vec::new());
    }
    let mut records = Vec::new();
    let mut installs = Vec::new();
    let source = inventory.source_identity()?;
    let instances: BTreeSet<_> = inventory
        .instances()
        .iter()
        .map(|instance| instance.legacy_id.as_str())
        .collect();
    for entry in decode_journal(&inventory.record_bytes(JOURNAL)?)? {
        match &entry.intent {
            LegacyIntent::Generic {} => match entry.command.as_str() {
                "RefreshPerformanceRules" => records.push(entry.convert_rules()?),
                "InstallVersion" | "ModifyInstanceContent" => {
                    let operation = entry.convert_install(&source)?;
                    if operation
                        .legacy_instance_id()
                        .is_some_and(|id| !instances.contains(id))
                    {
                        return Err(ImportError::InvalidData);
                    }
                    installs.push(operation);
                }
                _ => return Err(ImportError::InvalidData),
            },
            LegacyIntent::Performance(_) => {
                PreparedOperationImport::prepare(vec![entry.convert(&source)?])
                    .map_err(|_| ImportError::InvalidData)?;
            }
        }
    }
    PreparedInstallImport::validate(&installs).map_err(|_| ImportError::InvalidData)?;
    records.sort_by_key(|record| record.sequence);
    Ok(records)
}

impl LegacyOperation {
    fn convert_install(self, source: &str) -> ImportResult<PreparedInstallOperation> {
        if !matches!(self.intent, LegacyIntent::Generic {}) {
            return Err(ImportError::InvalidData);
        }
        PreparedInstallOperation::prepare(
            source,
            SourceOperation {
                journal_id: self.journal_id,
                operation_id: self.operation_id,
                sequence: self.sequence,
                parent_operation_id: self.parent_operation_id,
                command: self.command,
                intent: SourceIntent::Generic {},
                status: self.status,
                owner: self.owner,
                ownership: self.ownership,
                targets: self.targets,
                planned_steps: self
                    .planned_steps
                    .into_iter()
                    .map(LegacyGuardianStep::into_install)
                    .collect(),
                completed_steps: self
                    .completed_steps
                    .into_iter()
                    .map(LegacyGuardianStep::into_install)
                    .collect(),
                failure_point: self.failure_point,
                rollback: rollback_label(self.rollback).to_owned(),
                guardian_diagnosis_ids: self.guardian_diagnosis_ids,
                outcome: self.outcome,
                reconciliation_attempt: self.reconciliation_attempt,
                reconciliation_terminal: self.reconciliation_terminal,
                persisted_state_repair_attempt: self.persisted_state_repair_attempt,
                persisted_state_repair_terminal: self.persisted_state_repair_terminal,
                guardian_install_terminal: self.guardian_install_terminal,
            },
        )
        .map_err(|_| ImportError::InvalidData)
    }

    fn convert_rules(self) -> ImportResult<HistoricalRulesRefresh> {
        let cache_target = |target: &LegacyPerformanceTarget| {
            target.system == "Performance"
                && target.kind == "Config"
                && target.id == "performance_rules_cache"
                && target.ownership == "LauncherManaged"
        };
        let changed = self
            .completed_steps
            .first()
            .and_then(|step| step.changed_target.as_ref());
        let changed = match changed {
            None => false,
            Some(target) => {
                if !cache_target(target) {
                    return Err(ImportError::InvalidData);
                }
                true
            }
        };
        let outcome = match (
            self.status.as_str(),
            self.outcome.as_deref(),
            self.failure_point.as_deref(),
        ) {
            ("Succeeded", Some("Succeeded"), None) => HistoricalRulesRefreshOutcome::Succeeded {
                cache_changed: changed,
            },
            ("Failed", Some("Failed"), Some(failure)) if !changed => {
                HistoricalRulesRefreshOutcome::Failed {
                    failure_point: match failure {
                        "refresh_remote_rules" => HistoricalRulesRefreshFailure::RemoteRules,
                        "refresh_rules_journal_reconciliation" => {
                            HistoricalRulesRefreshFailure::JournalReconciliation
                        }
                        _ => return Err(ImportError::InvalidData),
                    },
                }
            }
            _ => return Err(ImportError::InvalidData),
        };
        let valid_step = |step: &LegacyGuardianStep, result: &str| {
            step.step_id == "refresh_remote_rules"
                && step.phase == "Running"
                && step.result == result
                && step.generated_facts.is_empty()
                && step.rollback == HistoricalRollback::NotApplicable
                && step.guardian_fact_ids.is_empty()
                && step.metrics.is_none()
        };
        if !matches!(self.intent, LegacyIntent::Generic {})
            || self.journal_id != format!("journal-{}", self.operation_id)
            || self.parent_operation_id.is_some()
            || self.command != "RefreshPerformanceRules"
            || self.owner != "Application"
            || self.ownership != "LauncherManaged"
            || self.rollback != HistoricalRollback::NotApplicable
            || self.targets.len() != 2
            || !self.targets.iter().any(cache_target)
            || !self.targets.iter().any(|target| {
                target.system == "Performance"
                    && target.kind == "NetworkResource"
                    && target.id == "performance_rules_remote_source"
                    && target.ownership == "ExternalProviderDerived"
            })
            || self.planned_steps.len() != 1
            || !self
                .planned_steps
                .iter()
                .all(|step| valid_step(step, "Planned") && step.changed_target.is_none())
            || self.completed_steps.len() != 1
            || !self.completed_steps.iter().all(|step| {
                valid_step(
                    step,
                    if self.status == "Succeeded" {
                        "Completed"
                    } else {
                        "Failed"
                    },
                )
            })
            || !self.guardian_diagnosis_ids.is_empty()
            || self.reconciliation_attempt.is_some()
            || self.reconciliation_terminal.is_some()
            || self.persisted_state_repair_attempt.is_some()
            || self.persisted_state_repair_terminal.is_some()
            || self.guardian_install_terminal.is_some()
        {
            return Err(ImportError::InvalidData);
        }
        Ok(HistoricalRulesRefresh {
            operation_id: self.operation_id,
            sequence: self.sequence,
            outcome,
        })
    }

    fn convert(self, source: &str) -> ImportResult<HistoricalOperation> {
        let LegacyIntent::Performance(lifecycle) = self.intent else {
            return Err(ImportError::InvalidData);
        };
        let LegacyPerformanceTerminal::Terminal { terminal } = lifecycle.phase;
        let (status, failure) = match &terminal {
            HistoricalOperationTerminal::Succeeded { .. } => ("Succeeded", None),
            HistoricalOperationTerminal::FailedBeforeEffect { .. }
            | HistoricalOperationTerminal::FailedAfterEffect { .. } => {
                ("Failed", Some("performance_operation_failed"))
            }
            HistoricalOperationTerminal::AbandonedBeforeEffect {} => (
                "Cancelled",
                Some("performance_operation_abandoned_before_effect"),
            ),
        };
        let intent = &lifecycle.intent;
        if self.journal_id != format!("journal-{}", self.operation_id)
            || self.parent_operation_id.is_some()
            || self.command != "ApplyPerformancePlan"
            || self.owner != "Application"
            || self.ownership != "CompositionManaged"
            || self.status != status
            || self.outcome.as_deref() != Some(status)
            || self.failure_point.as_deref() != failure
            || self.rollback != terminal.rollback(intent.rollback)
            || self.targets.len() != 2
            || !self.targets.iter().any(|target| {
                target.system == "State"
                    && target.kind == "Instance"
                    && target.id == intent.instance_id
                    && target.ownership == "CompositionManaged"
            })
            || !self.targets.iter().any(|target| {
                target.system == "Performance"
                    && target.kind == "PerformanceComposition"
                    && target.id == intent.base_target_id
                    && target.ownership == "CompositionManaged"
            })
            || !self.planned_steps.is_empty()
            || !excluded_guardian_ids(&self.guardian_diagnosis_ids, 32)
            || self.completed_steps.len() > 1
            || self.completed_steps.iter().any(|step| {
                step.step_id != "guardian_evidence"
                    || step.phase != "Running"
                    || step.result != "Completed"
                    || step.changed_target.is_some()
                    || !step.generated_facts.is_empty()
                    || step.rollback != HistoricalRollback::NotApplicable
                    || step.metrics.is_some()
                    || step.guardian_fact_ids.is_empty()
                    || !excluded_guardian_ids(&step.guardian_fact_ids, 64)
            })
            || self.reconciliation_attempt.is_some()
            || self.reconciliation_terminal.is_some()
            || self.persisted_state_repair_attempt.is_some()
            || self.persisted_state_repair_terminal.is_some()
            || self.guardian_install_terminal.is_some()
        {
            return Err(ImportError::InvalidData);
        }
        let mut hash = Sha256::new();
        hash.update(b"axial.legacy.performance.v1\0");
        hash.update((source.len() as u64).to_be_bytes());
        hash.update(source.as_bytes());
        hash.update(self.operation_id.as_bytes());
        Ok(HistoricalOperation {
            id: format!("legacy-performance-{:x}", hash.finalize()),
            instance_id: UNBOUND_INSTANCE
                .parse()
                .map_err(|_| ImportError::InvalidData)?,
            created_at: lifecycle.created_at,
            updated_at: lifecycle.updated_at,
            evidence: HistoricalOperationEvidence {
                operation_id: self.operation_id,
                sequence: self.sequence,
                intent: lifecycle.intent,
                terminal,
            },
        })
    }
}

// These known Guardian-only fields carry enum labels, not retained non-Guardian
// effects. Validate bounded shape before omitting them; do not interpret policy.
fn excluded_guardian_ids(values: &[String], limit: usize) -> bool {
    values.len() <= limit
        && values.iter().collect::<BTreeSet<_>>().len() == values.len()
        && values.iter().all(|value| {
            !value.is_empty()
                && value.len() <= 128
                && value
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        })
}

struct SourceReport {
    instance_id: String,
    launched_at: String,
    scenario: LaunchProofScenario,
    original_state: String,
    terminal_state: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacySuite {
    schema: String,
    schema_version: u32,
    suite_id: String,
    instance_id: String,
    mode: String,
    created_at: String,
    updated_at: String,
    runs: Vec<LegacySuiteRun>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacySuiteRun {
    run_index: usize,
    profile: String,
    run_type: String,
    target_id: String,
    benchmark_id: String,
    session_id: Option<String>,
    launched_at: Option<String>,
    state: String,
}

impl LegacySuite {
    fn convert(
        &self,
        source: &str,
        reports: &BTreeMap<String, SourceReport>,
        claimed_sessions: &mut BTreeSet<String>,
    ) -> ImportResult<BenchmarkSuiteManifest> {
        let created = benchmark_timestamp(&self.created_at)?;
        let updated = benchmark_timestamp(&self.updated_at)?;
        if self.schema != "axial.launch.benchmark.suite"
            || self.schema_version != 2
            || !suite_id(&self.suite_id)
            || !super::model::legacy_id(&self.instance_id)
            || !benchmark_mode(&self.mode)
            || created > updated
            || self.runs.is_empty()
            || self.runs.len() > 64
        {
            return Err(ImportError::InvalidData);
        }
        let mut indices = BTreeSet::new();
        let mut runs = Vec::with_capacity(self.runs.len());
        for run in &self.runs {
            if run.run_index >= 64
                || !indices.insert(run.run_index)
                || !manifest_field(&run.profile)
                || !manifest_field(&run.run_type)
                || !run.target_id.is_empty() && !manifest_field(&run.target_id)
                || !run
                    .benchmark_id
                    .strip_prefix("benchmark-")
                    .is_some_and(super::model::legacy_id)
            {
                return Err(ImportError::InvalidData);
            }
            let (session_id, launched_at) = match (
                run.state.as_str(),
                run.session_id.as_deref(),
                run.launched_at.as_deref(),
            ) {
                ("pending", None, None) => (None, None),
                ("failed" | "stopped" | "exited" | "completed", Some(session), Some(launched)) => {
                    let proof = reports.get(session).ok_or(ImportError::InvalidData)?;
                    if !session_id(session)
                        || !claimed_sessions.insert(session.to_owned())
                        || !proof.matches(self, run)
                        || benchmark_timestamp(launched)?
                            != benchmark_timestamp(&proof.launched_at)?
                        || benchmark_timestamp(launched)? > updated
                        || !(run.state == proof.terminal_state || run.state == proof.original_state)
                    {
                        return Err(ImportError::InvalidData);
                    }
                    (
                        Some(imported_id(source, session)),
                        Some(launched.to_owned()),
                    )
                }
                _ => return Err(ImportError::InvalidData),
            };
            runs.push(BenchmarkSuiteManifestRun {
                run_index: run.run_index,
                profile: run.profile.clone(),
                run_type: run.run_type.clone(),
                target_id: run.target_id.clone(),
                benchmark_id: run.benchmark_id.clone(),
                session_id,
                launched_at,
                state: run.state.clone(),
                launch_intent: None,
            });
        }
        // Legacy readers order by the preserved index; sparse plans stay sparse.
        runs.sort_by_key(|run| run.run_index);
        Ok(BenchmarkSuiteManifest {
            schema: self.schema.clone(),
            schema_version: self.schema_version,
            suite_id: imported_benchmark_id(source, "suite", &self.suite_id),
            instance_id: UNBOUND_INSTANCE.into(),
            mode: self.mode.clone(),
            created_at: self.created_at.clone(),
            updated_at: self.updated_at.clone(),
            runs,
            historical: true,
        })
    }
}

impl SourceReport {
    fn matches(&self, suite: &LegacySuite, run: &LegacySuiteRun) -> bool {
        self.instance_id == suite.instance_id
            && self.scenario.benchmark_profile.as_deref() == Some(run.profile.as_str())
            && self.scenario.benchmark_run_type.as_deref() == Some(run.run_type.as_str())
            && self.scenario.benchmark_mode.as_deref() == Some(suite.mode.as_str())
            && self.scenario.benchmark_id.as_deref() == Some(run.benchmark_id.as_str())
    }
}

/// The predecessor persists this status directly, without a schema envelope.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyDriver {
    id: String,
    suite_id: String,
    mode: String,
    state: String,
    interval_ms: u64,
    run_count: usize,
    launched_run_count: usize,
    pending_run_index: Option<usize>,
    active_session_id: Option<String>,
    last_run_index: Option<usize>,
    last_session_id: Option<String>,
    error: Option<String>,
    created_at: String,
    updated_at: String,
}

impl LegacyDriver {
    fn convert(
        self,
        source: &str,
        suite: &LegacySuite,
        reports: &BTreeMap<String, SourceReport>,
    ) -> ImportResult<BenchmarkSuiteDriverStatus> {
        let created = benchmark_timestamp(&self.created_at)?;
        let updated = benchmark_timestamp(&self.updated_at)?;
        if !self
            .id
            .strip_prefix("benchmark-suite-driver-")
            .is_some_and(super::model::legacy_id)
            || self.suite_id != suite.suite_id
            || self.mode != suite.mode
            || !matches!(
                self.state.as_str(),
                "complete" | "failed" | "stopped" | "interrupted"
            )
            || !(5_000..=3_600_000).contains(&self.interval_ms)
            || !(1..=64).contains(&self.run_count)
            || self.launched_run_count > self.run_count
            || self.active_session_id.is_some()
            || self.state == "complete" && self.pending_run_index.is_some()
            || self.last_session_id.is_some() && self.last_run_index.is_none()
            || created > updated
            || self.error.as_deref().is_some_and(|error| {
                error.chars().count() > 160
                    || !detail(error)
                    || error.chars().any(char::is_control)
                    || (matches!(
                        error,
                        "driver automatic resume queued after restart"
                            | "driver automatic resume started after restart"
                            | "driver ignored after restart resume limit"
                    ) && self.state != "interrupted")
            })
        {
            return Err(ImportError::InvalidData);
        }
        // A stopped driver may have planned the next, not-yet-created index.
        // Only a historical last run requires a surviving suite descriptor.
        if self
            .pending_run_index
            .is_some_and(|index| index >= self.run_count)
            || self.last_run_index.is_some_and(|index| {
                index >= self.run_count || !suite.runs.iter().any(|run| run.run_index == index)
            })
        {
            return Err(ImportError::InvalidData);
        }
        if let Some(session) = self.last_session_id.as_deref() {
            let run = suite
                .runs
                .iter()
                .find(|run| Some(run.run_index) == self.last_run_index)
                .ok_or(ImportError::InvalidData)?;
            let proof = reports.get(session).ok_or(ImportError::InvalidData)?;
            // A terminal driver may predate an explicit rerun. Preserve its
            // counts and old session when the retained descriptor proves it.
            if !session_id(session)
                || !proof.matches(suite, run)
                || benchmark_timestamp(&proof.launched_at)? > updated
            {
                return Err(ImportError::InvalidData);
            }
        }
        Ok(BenchmarkSuiteDriverStatus {
            id: imported_benchmark_id(source, "driver", &self.id),
            suite_id: imported_benchmark_id(source, "suite", &self.suite_id),
            mode: self.mode,
            state: self.state,
            interval_ms: self.interval_ms,
            run_count: self.run_count,
            launched_run_count: self.launched_run_count,
            pending_run_index: self.pending_run_index,
            active_session_id: None,
            last_run_index: self.last_run_index,
            last_session_id: self.last_session_id.map(|id| imported_id(source, &id)),
            error: self.error,
            created_at: self.created_at,
            updated_at: self.updated_at,
            historical: true,
        })
    }
}

fn benchmark_mode(value: &str) -> bool {
    matches!(
        value,
        "development" | "qualification" | "release_validation"
    )
}

fn manifest_field(value: &str) -> bool {
    token(value) && !value.contains(':')
}

fn suite_id(value: &str) -> bool {
    value
        .strip_prefix("suite-")
        .and_then(|value| value.rsplit_once('-'))
        .is_some_and(|(mode, id)| {
            matches!(mode, "dev" | "qual" | "release" | "custom") && super::model::legacy_id(id)
        })
}

fn benchmark_timestamp(value: &str) -> ImportResult<chrono::DateTime<chrono::Utc>> {
    let time = chrono::DateTime::parse_from_rfc3339(value)
        .map_err(|_| ImportError::InvalidData)?
        .with_timezone(&chrono::Utc);
    if time.to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true) != value
        && time.to_rfc3339_opts(chrono::SecondsFormat::Millis, true) != value
    {
        return Err(ImportError::InvalidData);
    }
    Ok(time)
}

fn imported_benchmark_id(source: &str, kind: &str, id: &str) -> String {
    let mut hash = Sha256::new();
    hash.update(b"axial.legacy.benchmark.v2\0");
    for value in [kind, source, id] {
        hash.update((value.len() as u64).to_be_bytes());
        hash.update(value.as_bytes());
    }
    format!("legacy-{kind}-{:x}", hash.finalize())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyReport {
    schema: String,
    schema_version: u32,
    session_id: String,
    instance_id: String,
    version_id: String,
    launched_at: String,
    recorded_at: String,
    outcome: String,
    session_outcome: Option<LegacyOutcome>,
    scenario: LaunchProofScenario,
    device: LaunchProofDevice,
    resource_budget: Option<LaunchProofResourceBudget>,
    pid: Option<u32>,
    exit_code: Option<i32>,
    boot_duration_ms: Option<u64>,
    priority: Option<LegacyPriority>,
    failure_class: Option<FailureClass>,
    failure_detail: Option<String>,
    crash_evidence: Option<CrashEvidence>,
    #[serde(rename = "guardian")]
    _guardian: Option<serde::de::IgnoredAny>,
    #[serde(rename = "healing")]
    _healing: Option<serde::de::IgnoredAny>,
    stages: Vec<LaunchProofStage>,
    comparison: Option<LaunchProofComparison>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyOutcome {
    reason: String,
    kind: SessionOutcomeKind,
    summary: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyPriority {
    start_mode: String,
    start_error: Option<String>,
    promotion: Option<String>,
    promotion_error: Option<String>,
}

impl LegacyReport {
    fn source_proof(&self) -> SourceReport {
        let (original_state, terminal_state) = crate::launch::reports::imported_suite_states(
            &self.outcome,
            self.session_outcome.as_ref().map(|value| value.kind),
        );
        SourceReport {
            instance_id: self.instance_id.clone(),
            launched_at: self.launched_at.clone(),
            scenario: self.scenario.clone(),
            original_state,
            terminal_state,
        }
    }

    fn convert(mut self, source: &str, filename: &str) -> ImportResult<LaunchProofRecord> {
        if self.schema != "axial.launch.proof"
            || self.schema_version != 3
            || !session_id(&self.session_id)
            || filename != format!("{}.json", self.session_id)
            || !super::model::legacy_id(&self.instance_id)
            || !token(&self.version_id)
            || !timestamp(&self.launched_at)
            || !timestamp(&self.recorded_at)
            || self.recorded_at < self.launched_at
            || !scenario_valid(&self.scenario)
            || !matches!(
                self.device.tier.as_str(),
                "low" | "mid" | "high" | "unknown"
            )
            || self.device.total_memory_mb == Some(0)
            || self.device.cpu_threads == Some(0)
            || self.stages.len() > 32
            || !self.stages.iter().all(stage_valid)
            || self
                .failure_detail
                .as_deref()
                .is_some_and(|value| !detail(value))
        {
            return Err(ImportError::InvalidData);
        }
        let mut outcome = self
            .session_outcome
            .as_ref()
            .map(LegacyOutcome::convert)
            .transpose()?
            .unwrap_or(SessionOutcome {
                kind: SessionOutcomeKind::Unknown,
                reason: SessionExitReason::UnknownExit,
                failure_class: None,
                summary: String::new(),
            });
        if !crate::launch::reports::imported_outcome_matches(
            &self.outcome,
            self.session_outcome.as_ref().map(|value| value.kind),
        ) || self.crash_evidence.is_some()
            && !(matches!(self.outcome.as_str(), "failed" | "exited")
                && self.session_outcome.as_ref().is_some_and(|value| {
                    matches!(
                        value.kind,
                        SessionOutcomeKind::Failed | SessionOutcomeKind::Unknown
                    )
                }))
        {
            return Err(ImportError::InvalidData);
        }
        if let Some(comparison) = &self.comparison {
            if !comparison_valid(&self, comparison) {
                return Err(ImportError::InvalidData);
            }
        }
        outcome.failure_class = self.failure_class;
        outcome.summary = outcome.summary().to_owned();
        let mut evidence = vec![
            historical(
                "original_session",
                "Original session",
                self.session_id.clone(),
            ),
            historical(
                "original_outcome",
                "Original terminal report outcome",
                self.outcome.clone(),
            ),
        ];
        if let Some(value) = self.pid {
            evidence.push(historical(
                "original_pid",
                "Historical process identifier",
                value.to_string(),
            ));
        }
        if let Some(value) = &self.session_outcome {
            evidence.push(historical(
                "original_reason",
                "Original exit reason",
                value.reason.clone(),
            ));
        }
        if let Some(priority) = self.priority {
            if !token(&priority.start_mode)
                || priority
                    .promotion
                    .as_deref()
                    .is_some_and(|value| !token(value))
                || priority
                    .start_error
                    .as_deref()
                    .is_some_and(|value| !detail(value))
                || priority
                    .promotion_error
                    .as_deref()
                    .is_some_and(|value| !detail(value))
            {
                return Err(ImportError::InvalidData);
            }
            evidence.push(historical(
                "priority_start",
                "Initial process priority",
                priority.start_mode,
            ));
            for (id, label, value) in [
                (
                    "priority_start_error",
                    "Initial priority error",
                    priority.start_error,
                ),
                (
                    "priority_promotion",
                    "Process priority promotion",
                    priority.promotion,
                ),
                (
                    "priority_promotion_error",
                    "Priority promotion error",
                    priority.promotion_error,
                ),
            ] {
                if let Some(value) = value {
                    evidence.push(historical(id, label, value));
                }
            }
        }
        if let Some(value) = self.failure_detail {
            evidence.push(historical(
                "failure_detail",
                "Historical failure detail",
                value,
            ));
        }
        self.stages.retain(|stage| !excluded_system(&stage.stage));
        for stage in &mut self.stages {
            stage.evidence.retain(|item| !excluded_system(&item.system));
        }
        if let Some(comparison) = &self.comparison {
            if comparison.metric_name == "total_completed_stage_duration_ms"
                && total_duration(&self.stages) != Some(comparison.current_value_ms)
            {
                return Err(ImportError::InvalidData);
            }
        }
        for items in evidence.chunks(4) {
            self.stages.push(LaunchProofStage {
                stage: "imported_history".into(),
                label: "Imported terminal evidence".into(),
                started_at_ms: 0,
                ended_at_ms: None,
                duration_ms: None,
                result: None,
                warnings: Vec::new(),
                fallback_reason: None,
                evidence: items.to_vec(),
            });
        }
        if self.stages.len() > 32 {
            return Err(ImportError::LimitExceeded);
        }
        if let Some(comparison) = &mut self.comparison {
            comparison.baseline_session_id = imported_id(source, &comparison.baseline_session_id);
            comparison.delta_percent =
                comparison.delta_ms as f64 / comparison.baseline_value_ms as f64 * 100.0;
        }
        Ok(LaunchProofRecord {
            schema: self.schema,
            schema_version: 4,
            session_id: imported_id(source, &self.session_id),
            instance_id: UNBOUND_INSTANCE.into(),
            version_id: self.version_id,
            launched_at: self.launched_at,
            recorded_at: self.recorded_at,
            outcome: match outcome.kind {
                SessionOutcomeKind::Clean => "exited",
                SessionOutcomeKind::Stopped => "stopped",
                SessionOutcomeKind::Failed => "failed",
                SessionOutcomeKind::Unknown => "unknown",
            }
            .into(),
            session_outcome: outcome,
            scenario: self.scenario,
            device: self.device,
            resource_budget: self.resource_budget,
            exit_code: self.exit_code,
            boot_duration_ms: self.boot_duration_ms,
            crash_evidence: self.crash_evidence,
            stages: self.stages,
            comparison: self.comparison,
            logs: Vec::new(),
            logs_dropped: 0,
        })
    }
}

impl LegacyOutcome {
    fn convert(&self) -> ImportResult<SessionOutcome> {
        use SessionExitReason::*;
        let (reason, kind, summary) = match self.reason.as_str() {
            "clean_exit" => (
                CleanExit,
                SessionOutcomeKind::Clean,
                "Minecraft exited cleanly.",
            ),
            "external_user_closed" => (
                ExternalUserClosed,
                SessionOutcomeKind::Clean,
                "Minecraft was closed outside the launcher after startup.",
            ),
            "launcher_stopped" => (
                LauncherStopped,
                SessionOutcomeKind::Stopped,
                "The launcher stopped the session.",
            ),
            "spawn_failed" => (
                SpawnFailed,
                SessionOutcomeKind::Failed,
                "The game process could not be started.",
            ),
            "startup_failed" => (
                StartupFailed,
                SessionOutcomeKind::Failed,
                "Minecraft failed during startup.",
            ),
            "startup_stalled" | "watchdog_killed" => (
                StartupStalled,
                SessionOutcomeKind::Failed,
                "Minecraft did not finish startup in time.",
            ),
            "crashed_before_boot" => (
                CrashedBeforeBoot,
                SessionOutcomeKind::Failed,
                "Minecraft exited before startup completed.",
            ),
            "crashed_after_boot" => (
                CrashedAfterBoot,
                SessionOutcomeKind::Failed,
                "Minecraft crashed after startup.",
            ),
            "unknown_exit" => (
                UnknownExit,
                SessionOutcomeKind::Unknown,
                "Minecraft exited and the launcher could not classify the reason.",
            ),
            _ => return Err(ImportError::InvalidData),
        };
        if self.kind != kind || self.summary != summary {
            return Err(ImportError::InvalidData);
        }
        Ok(SessionOutcome {
            reason,
            kind,
            failure_class: None,
            summary: String::new(),
        })
    }
}

fn imported_id(source: &str, session: &str) -> String {
    let mut hash = Sha256::new();
    hash.update(b"axial.legacy.launch.v3\0");
    hash.update((source.len() as u64).to_be_bytes());
    hash.update(source.as_bytes());
    hash.update(session.as_bytes());
    format!("legacy-{:x}", hash.finalize())
}

fn historical(id: &str, label: &str, value: String) -> LaunchProofStageEvidence {
    LaunchProofStageEvidence {
        id: id.into(),
        system: "history".into(),
        summary: label.into(),
        details: vec![value],
    }
}

fn excluded_system(value: &str) -> bool {
    matches!(value, "guardian" | "healing")
}
fn session_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 96
        && value
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || b"_-".contains(&c))
}
fn token(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 96
        && value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"_.-+:".contains(&c))
        && Redactor::new(Vec::new()).redact_line(value) == value
}
fn detail(value: &str) -> bool {
    !value.is_empty()
        && value.chars().count() <= 180
        && !value.contains(['/', '\\'])
        && Redactor::new(Vec::new()).redact_line(value) == value
}
fn timestamp(value: &str) -> bool {
    chrono::DateTime::parse_from_rfc3339(value).is_ok_and(|time| {
        time.with_timezone(&chrono::Utc)
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
            == value
    })
}
fn scenario_valid(value: &LaunchProofScenario) -> bool {
    let scenario = match value.performance_mode.as_str() {
        "managed" => "managed_launch",
        "vanilla" => "vanilla_launch",
        "custom" => "custom_launch",
        "unknown" => "unknown_launch",
        _ => return false,
    };
    value.scenario_id == scenario
        && value.requested_memory_mb.is_none_or(|memory| memory > 0)
        && [
            &value.version_id,
            &value.benchmark_profile,
            &value.benchmark_run_type,
            &value.benchmark_id,
        ]
        .into_iter()
        .all(|value| value.as_deref().is_none_or(token))
        && value.benchmark_mode.as_deref().is_none_or(|mode| {
            matches!(mode, "development" | "qualification" | "release_validation")
        })
}
fn stage_valid(stage: &LaunchProofStage) -> bool {
    token(&stage.stage)
        && detail(&stage.label)
        && stage.result.as_deref().is_none_or(token)
        && stage.fallback_reason.as_deref().is_none_or(detail)
        && stage.warnings.len() <= 8
        && stage.warnings.iter().all(|value| detail(value))
        && stage.evidence.len() <= 4
        && stage.evidence.iter().all(|item| {
            token(&item.id)
                && token(&item.system)
                && detail(&item.summary)
                && item.details.len() <= 4
                && item.details.iter().all(|value| detail(value))
        })
        && match (stage.ended_at_ms, stage.duration_ms) {
            (Some(end), Some(duration)) => {
                end >= stage.started_at_ms && end - stage.started_at_ms == duration
            }
            (None, None) => true,
            _ => false,
        }
}
fn total_duration(stages: &[LaunchProofStage]) -> Option<u64> {
    stages
        .iter()
        .filter_map(|stage| stage.duration_ms)
        .reduce(u64::saturating_add)
}

fn comparison_valid(report: &LegacyReport, comparison: &LaunchProofComparison) -> bool {
    let baseline = &comparison.baseline;
    let delta = if comparison.current_value_ms >= comparison.baseline_value_ms {
        i64::try_from(comparison.current_value_ms - comparison.baseline_value_ms)
            .unwrap_or(i64::MAX)
    } else {
        -i64::try_from(comparison.baseline_value_ms - comparison.current_value_ms)
            .unwrap_or(i64::MAX)
    };
    let percent = delta as f64 / comparison.baseline_value_ms as f64 * 100.0;
    let metric = match comparison.metric_name.as_str() {
        "boot_duration_ms" => report.boot_duration_ms,
        "total_completed_stage_duration_ms" => total_duration(&report.stages),
        _ => return false,
    };
    session_id(&comparison.baseline_session_id)
        && comparison.baseline_session_id != report.session_id
        && timestamp(&comparison.baseline_recorded_at)
        && (
            &comparison.baseline_recorded_at,
            &comparison.baseline_session_id,
        ) < (&report.recorded_at, &report.session_id)
        && matches!(
            (
                report.scenario.performance_mode.as_str(),
                baseline.performance_mode.as_str()
            ),
            ("managed", "managed" | "vanilla") | ("vanilla", "vanilla") | ("custom", "custom")
        )
        && baseline.version_id != "unknown"
        && token(&baseline.version_id)
        && report
            .scenario
            .version_id
            .as_deref()
            .filter(|value| *value != "unknown")
            .unwrap_or(&report.version_id)
            == baseline.version_id
        && baseline.requested_memory_mb == report.scenario.requested_memory_mb
        && baseline.device_tier != "unknown"
        && baseline.device_tier == report.device.tier
        && optional_dimension(&baseline.benchmark_profile)
            == optional_dimension(&report.scenario.benchmark_profile)
        && optional_dimension(&baseline.benchmark_run_type)
            == optional_dimension(&report.scenario.benchmark_run_type)
        && baseline.benchmark_mode == report.scenario.benchmark_mode
        && comparison.matched_sample_count > 0
        && comparison.matched_sample_count <= MAX_HISTORY_RECORDS
        && comparison.current_value_ms > 0
        && comparison.baseline_value_ms > 0
        && metric == Some(comparison.current_value_ms)
        && comparison.delta_ms == delta
        && comparison.delta_percent.is_finite()
        && (comparison.delta_percent - percent).abs() <= f64::EPSILON * percent.abs().max(1.0) * 4.0
}

fn optional_dimension(value: &Option<String>) -> Option<&str> {
    value.as_deref().filter(|value| *value != "unknown")
}

#[cfg(test)]
#[path = "history_tests.rs"]
mod tests;
