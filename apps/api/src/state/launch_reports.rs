use crate::execution::file::{DeleteFileRequest, delete_launcher_managed_file};
use crate::execution::persistence::{
    AcceptedWrite, AtomicSnapshotWriter, PersistenceCoordinator, PersistenceOwnerLease,
    WriteUrgency,
};
use crate::logging::timestamp_utc;
use crate::observability::{RedactionAudience, sanitize_evidence_text, sanitize_evidence_token};
use crate::state::benchmark_suites::{
    BenchmarkProofRetentionHandle, MAX_BENCHMARK_PROOF_SESSION_IDS,
};
use crate::state::contracts::{OwnershipClass, StabilizationSystem, TargetDescriptor, TargetKind};
use axial_config::AppPaths;
use axial_launcher::{
    GuardianSummary, LaunchHealingSummary, LaunchIntent, LaunchPriorityEvidence,
    LaunchSessionOutcome, LaunchSessionOutcomeKind, LaunchSessionRecord, LaunchStageEvidence,
    LaunchStageRecord, launch_state_name,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs::{self, File};
use std::io;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use sysinfo::System;
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};

const LAUNCH_PROOF_SCHEMA: &str = "axial.launch.proof";
const LAUNCH_PROOF_SCHEMA_VERSION: u32 = 2;
const LAUNCH_STAGE_COMPARISON_METRIC_NAME: &str = "total_completed_stage_duration_ms";
const LAUNCH_BOOT_COMPARISON_METRIC_NAME: &str = "boot_duration_ms";
const MAX_REPORT_FILENAME_STEM: usize = 96;
const MAX_BENCHMARK_METADATA_CHARS: usize = 96;
const MAX_EXPORT_TOKEN_CHARS: usize = 96;
const MAX_EXPORT_DETAIL_CHARS: usize = 180;
const MAX_EXPORT_DETAILS: usize = 8;
const MAX_EXPORT_STAGES: usize = 32;
const MAX_REPORT_STAGE_EVIDENCE: usize = 4;
const MAX_REPORT_EVIDENCE_DETAILS: usize = 4;
const MAX_REPORT_BYTES: u64 = 256 * 1024;
const MAX_STARTUP_REPORTS: usize = MAX_BENCHMARK_PROOF_SESSION_IDS;
const MAX_LOAD_ISSUES: usize = 8;
const LAUNCH_REPORT_STORE_LOCK_INVARIANT: &str =
    "launch report store lock poisoned; committed and persisted state may diverge";
type LaunchComparisonMetric = (&'static str, u64, fn(&LaunchProofRecord) -> Option<u64>);

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub(crate) struct LaunchProofRecord {
    pub schema: String,
    pub schema_version: u32,
    pub session_id: String,
    pub instance_id: String,
    pub version_id: String,
    pub launched_at: String,
    pub recorded_at: String,
    pub outcome: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_outcome: Option<LaunchSessionOutcome>,
    pub scenario: LaunchProofScenario,
    pub device: LaunchProofDevice,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resource_budget: Option<LaunchProofResourceBudget>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub boot_duration_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub priority: Option<LaunchProofPriority>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure_class: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure_detail: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub guardian: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub healing: Option<Value>,
    pub stages: Vec<LaunchStageRecord>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub comparison: Option<LaunchProofComparison>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct LaunchProofExport {
    pub schema: String,
    pub schema_version: u32,
    pub session_id: String,
    pub instance_id: String,
    pub version_id: String,
    pub launched_at: String,
    pub recorded_at: String,
    pub outcome: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_outcome: Option<LaunchSessionOutcome>,
    pub scenario: LaunchProofScenario,
    pub device: LaunchProofDevice,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resource_budget: Option<LaunchProofResourceBudget>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub boot_duration_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure_class: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub guardian: Option<GuardianSummary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub healing: Option<LaunchHealingSummary>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stages: Vec<LaunchProofStageExport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub comparison: Option<LaunchProofComparison>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(crate) struct LaunchProofStageExport {
    pub stage: String,
    pub label: String,
    pub started_at_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ended_at_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fallback_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence: Vec<LaunchProofStageEvidenceExport>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(crate) struct LaunchProofStageEvidenceExport {
    pub id: String,
    pub system: String,
    pub summary: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub details: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct LaunchProofScenario {
    pub scenario_id: String,
    pub performance_mode: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requested_memory_mb: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub benchmark_profile: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub benchmark_run_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub benchmark_mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub benchmark_id: Option<String>,
}

impl Default for LaunchProofScenario {
    fn default() -> Self {
        Self {
            scenario_id: "unknown_launch".to_string(),
            performance_mode: "unknown".to_string(),
            requested_memory_mb: None,
            version_id: None,
            benchmark_profile: None,
            benchmark_run_type: None,
            benchmark_mode: None,
            benchmark_id: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct LaunchProofDevice {
    pub tier: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_memory_mb: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu_threads: Option<usize>,
}

impl Default for LaunchProofDevice {
    fn default() -> Self {
        Self {
            tier: "unknown".to_string(),
            total_memory_mb: None,
            cpu_threads: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub(crate) struct LaunchProofComparison {
    pub baseline_session_id: String,
    pub baseline_recorded_at: String,
    pub baseline: LaunchProofComparisonBaseline,
    pub matched_sample_count: usize,
    pub metric_name: String,
    pub current_value_ms: u64,
    pub baseline_value_ms: u64,
    pub delta_ms: i64,
    pub delta_percent: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct LaunchProofComparisonBaseline {
    pub performance_mode: String,
    pub version_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requested_memory_mb: Option<i32>,
    pub device_tier: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub benchmark_profile: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub benchmark_run_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub benchmark_mode: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct LaunchProofResourceBudget {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host_total_memory_mb: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host_available_memory_mb: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host_used_memory_mb: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host_cpu_threads: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host_cpu_load_1m_x100: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host_cpu_load_5m_x100: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host_cpu_load_15m_x100: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub launcher_process_memory_mb: Option<u64>,
    pub active_session_count: usize,
    pub active_install_count: usize,
    pub active_memory_allocation_mb: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requested_memory_mb: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub estimated_remaining_memory_mb: Option<i64>,
    pub memory_headroom_mb: u64,
    pub memory_pressure: bool,
    pub cpu_pressure: bool,
    pub install_pressure: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub launch_disk_available_mb: Option<u64>,
    pub launch_disk_headroom_mb: u64,
    pub disk_pressure: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct LaunchProofPriority {
    pub start_mode: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub promotion: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub promotion_error: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(crate) struct LaunchBenchmarkMetadata {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub benchmark_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
}

impl LaunchBenchmarkMetadata {
    pub(crate) fn new(
        benchmark_id: Option<&str>,
        profile: Option<&str>,
        run_type: Option<&str>,
        mode: Option<&str>,
    ) -> Self {
        Self {
            benchmark_id: benchmark_id.and_then(sanitize_benchmark_metadata),
            profile: profile.and_then(sanitize_benchmark_metadata),
            run_type: run_type.and_then(sanitize_benchmark_metadata),
            mode: mode.and_then(sanitize_benchmark_mode_metadata),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LaunchProofContext {
    pub performance_mode: String,
    pub requested_memory_mb: Option<i32>,
    pub version_id: Option<String>,
    pub benchmark: Option<LaunchBenchmarkMetadata>,
    pub resource_budget: Option<LaunchProofResourceBudget>,
}

impl LaunchProofContext {
    pub(crate) fn from_intent(intent: &LaunchIntent) -> Self {
        Self {
            performance_mode: trimmed_or_unknown(&intent.performance_mode),
            requested_memory_mb: positive_i32(intent.max_memory_mb),
            version_id: non_empty_string(&intent.version_id),
            benchmark: None,
            resource_budget: None,
        }
    }

    pub(crate) fn with_benchmark(mut self, benchmark: Option<LaunchBenchmarkMetadata>) -> Self {
        self.benchmark = benchmark;
        self
    }

    pub(crate) fn with_resource_budget(
        mut self,
        resource_budget: Option<LaunchProofResourceBudget>,
    ) -> Self {
        self.resource_budget = resource_budget;
        self
    }
}

pub(crate) struct LaunchReportStore {
    state: Arc<Mutex<LaunchReportState>>,
    mutation_gate: Arc<AsyncMutex<()>>,
    persistence: Option<Arc<LaunchReportPersistence>>,
    proof_retention: Arc<RwLock<BenchmarkProofRetentionHandle>>,
}

struct LaunchReportState {
    reports: BTreeMap<String, LaunchProofRecord>,
    order: BTreeSet<(String, String)>,
    writers: BTreeMap<String, AtomicSnapshotWriter>,
    retry_candidate: Option<PendingLaunchReport>,
    cleanup_candidate: Option<String>,
    load_issue_count: usize,
    mutation_latched: bool,
}

struct LaunchReportPersistence {
    owner: PersistenceOwnerLease,
    directory: PathBuf,
}

#[derive(Clone)]
struct PendingLaunchReport {
    revision: u64,
    record: LaunchProofRecord,
}

struct AcceptedLaunchReport {
    ticket: AcceptedWrite,
    pending: PendingLaunchReport,
}

impl LaunchReportStore {
    pub(crate) fn load_from_paths(
        paths: &AppPaths,
        proof_retention: BenchmarkProofRetentionHandle,
    ) -> Self {
        Self::load_from_paths_with_coordinator_and_retention(
            paths,
            PersistenceCoordinator::global(),
            proof_retention,
        )
        .unwrap_or_else(|_| panic!("failed to initialize launch report persistence"))
    }

    #[cfg(test)]
    pub(crate) fn load_from_paths_for_test(paths: &AppPaths) -> Self {
        Self::load_from_paths(paths, BenchmarkProofRetentionHandle::empty())
    }

    #[cfg(test)]
    fn load_from_paths_with_coordinator(
        paths: &AppPaths,
        coordinator: PersistenceCoordinator,
    ) -> io::Result<Self> {
        Self::load_from_paths_with_coordinator_and_retention(
            paths,
            coordinator,
            BenchmarkProofRetentionHandle::empty(),
        )
    }

    fn load_from_paths_with_coordinator_and_retention(
        paths: &AppPaths,
        coordinator: PersistenceCoordinator,
        proof_retention: BenchmarkProofRetentionHandle,
    ) -> io::Result<Self> {
        let directory = report_dir(paths);
        let owner = coordinator
            .claim_owner(&directory)
            .map_err(io::Error::from)?;
        let (reports, load_issue_count) = match proof_retention
            .retained_session_ids(MAX_STARTUP_REPORTS)
        {
            Some(protected_session_ids) => load_report_index(&directory, &protected_session_ids),
            None => (BTreeMap::new(), 1),
        };
        let order = report_order(&reports);
        Ok(Self {
            state: Arc::new(Mutex::new(LaunchReportState {
                reports,
                order,
                writers: BTreeMap::new(),
                retry_candidate: None,
                cleanup_candidate: None,
                load_issue_count,
                mutation_latched: load_issue_count != 0,
            })),
            mutation_gate: Arc::new(AsyncMutex::new(())),
            persistence: Some(Arc::new(LaunchReportPersistence { owner, directory })),
            proof_retention: Arc::new(RwLock::new(proof_retention)),
        })
    }

    #[cfg(test)]
    fn in_memory() -> Self {
        Self {
            state: Arc::new(Mutex::new(LaunchReportState {
                reports: BTreeMap::new(),
                order: BTreeSet::new(),
                writers: BTreeMap::new(),
                retry_candidate: None,
                cleanup_candidate: None,
                load_issue_count: 0,
                mutation_latched: false,
            })),
            mutation_gate: Arc::new(AsyncMutex::new(())),
            persistence: None,
            proof_retention: Arc::new(RwLock::new(BenchmarkProofRetentionHandle::empty())),
        }
    }

    #[cfg(test)]
    pub(crate) fn bind_proof_retention(&self, proof_retention: BenchmarkProofRetentionHandle) {
        *self
            .proof_retention
            .write()
            .expect(LAUNCH_REPORT_STORE_LOCK_INVARIANT) = proof_retention;
    }

    pub(crate) fn load_issue_count(&self) -> usize {
        self.state
            .lock()
            .expect(LAUNCH_REPORT_STORE_LOCK_INVARIANT)
            .load_issue_count
    }

    pub(crate) fn list_recent(&self, limit: usize) -> Vec<LaunchProofRecord> {
        let state = self.state.lock().expect(LAUNCH_REPORT_STORE_LOCK_INVARIANT);
        list_recent_from_state(&state, limit)
    }

    pub(crate) fn list_recent_exports(&self, limit: usize) -> Vec<LaunchProofExport> {
        self.list_recent(limit)
            .iter()
            .map(LaunchProofExport::from_record)
            .collect()
    }

    pub(crate) fn load(&self, session_id: &str) -> Option<LaunchProofRecord> {
        self.state
            .lock()
            .expect(LAUNCH_REPORT_STORE_LOCK_INVARIANT)
            .reports
            .get(session_id)
            .cloned()
    }

    pub(crate) fn load_export(&self, session_id: &str) -> Option<LaunchProofExport> {
        self.load(session_id)
            .as_ref()
            .map(LaunchProofExport::from_record)
    }

    #[cfg(test)]
    pub(crate) fn insert_unchecked_for_test(&self, report: LaunchProofRecord) {
        insert_committed_report(
            &mut self.state.lock().expect(LAUNCH_REPORT_STORE_LOCK_INVARIANT),
            report,
        );
    }

    pub(crate) async fn persist(
        &self,
        record: LaunchSessionRecord,
        launched_at: Option<String>,
        outcome: String,
        context: Option<LaunchProofContext>,
    ) -> io::Result<LaunchProofRecord> {
        let mutation = self.mutation_gate.clone().lock_owned().await;
        let mutation = self.reconcile_retry_holding_gate(mutation).await?;
        let mutation = self.reconcile_cleanup_holding_gate(mutation).await?;
        if self
            .state
            .lock()
            .expect(LAUNCH_REPORT_STORE_LOCK_INVARIANT)
            .mutation_latched
        {
            return Err(invalid_report(
                "launch report mutation is unavailable after startup rejection",
            ));
        }
        let report_state = self.state.clone();
        let candidate = tokio::task::spawn_blocking(move || {
            let candidates = list_recent_from_state(
                &report_state
                    .lock()
                    .expect(LAUNCH_REPORT_STORE_LOCK_INVARIANT),
                MAX_STARTUP_REPORTS,
            );
            let mut proof =
                build_record(&record, launched_at.as_deref(), &outcome, context.as_ref());
            proof.comparison = build_comparison_from_candidates(&proof, &candidates);
            proof
        })
        .await
        .map_err(|_| io::Error::other("launch report construction task stopped"))?;
        validate_admitted_report(&candidate, &report_filename(&candidate.session_id))?;

        if let Some(current) = self.load(&candidate.session_id)
            && report_is_terminal(&current.outcome)
            && !report_is_terminal(&candidate.outcome)
        {
            return Ok(current);
        }
        self.commit(candidate, mutation).await
    }

    pub(crate) async fn close(&self) -> io::Result<()> {
        let mutation = self.mutation_gate.clone().lock_owned().await;
        let mutation = self.reconcile_retry_holding_gate(mutation).await?;
        let mutation = self.reconcile_cleanup_holding_gate(mutation).await?;
        if let Some(persistence) = &self.persistence {
            persistence.owner.close().await.map_err(io::Error::from)?;
        }
        drop(mutation);
        Ok(())
    }

    async fn reconcile_retry_holding_gate(
        &self,
        mutation: OwnedMutexGuard<()>,
    ) -> io::Result<OwnedMutexGuard<()>> {
        let retry = self
            .state
            .lock()
            .expect(LAUNCH_REPORT_STORE_LOCK_INVARIANT)
            .retry_candidate
            .clone();
        let Some(retry) = retry else {
            return Ok(mutation);
        };
        let writer = self.writer_for(&retry.record.session_id)?;
        let ticket = writer.retry().map_err(io::Error::from)?;
        assert_eq!(ticket.revision().get(), retry.revision);
        let (_, mutation) = self
            .await_commit_holding_gate(
                AcceptedLaunchReport {
                    ticket,
                    pending: retry,
                },
                mutation,
            )
            .await?;
        Ok(mutation)
    }

    async fn reconcile_cleanup_holding_gate(
        &self,
        mutation: OwnedMutexGuard<()>,
    ) -> io::Result<OwnedMutexGuard<()>> {
        let (result, mutation) = reconcile_launch_report_cleanup(
            self.state.clone(),
            self.persistence.clone(),
            self.proof_retention.clone(),
            mutation,
        )
        .await;
        result?;
        Ok(mutation)
    }

    async fn commit(
        &self,
        candidate: LaunchProofRecord,
        mutation: OwnedMutexGuard<()>,
    ) -> io::Result<LaunchProofRecord> {
        let Some(_) = &self.persistence else {
            insert_committed_report(
                &mut self.state.lock().expect(LAUNCH_REPORT_STORE_LOCK_INVARIANT),
                candidate.clone(),
            );
            let mutation = self.reconcile_cleanup_holding_gate(mutation).await?;
            drop(mutation);
            return Ok(candidate);
        };
        let writer = self.writer_for(&candidate.session_id)?;
        let ticket = writer
            .accept(
                candidate.clone(),
                WriteUrgency::Immediate,
                encode_launch_report,
            )
            .map_err(io::Error::from)?;
        let pending = PendingLaunchReport {
            revision: ticket.revision().get(),
            record: candidate,
        };
        let (record, _) = self
            .await_commit_holding_gate(AcceptedLaunchReport { ticket, pending }, mutation)
            .await?;
        Ok(record)
    }

    fn writer_for(&self, session_id: &str) -> io::Result<AtomicSnapshotWriter> {
        let mut state = self.state.lock().expect(LAUNCH_REPORT_STORE_LOCK_INVARIANT);
        if let Some(writer) = state.writers.get(session_id) {
            return Ok(writer.clone());
        }
        let persistence = self
            .persistence
            .as_ref()
            .ok_or_else(|| io::Error::other("launch report persistence unavailable"))?;
        let writer = persistence
            .owner
            .writer(
                report_path_in(&persistence.directory, session_id),
                launch_report_target(session_id),
            )
            .map_err(io::Error::from)?;
        state.writers.insert(session_id.to_string(), writer.clone());
        Ok(writer)
    }

    async fn await_commit_holding_gate(
        &self,
        accepted: AcceptedLaunchReport,
        mutation: OwnedMutexGuard<()>,
    ) -> io::Result<(LaunchProofRecord, OwnedMutexGuard<()>)> {
        let state = self.state.clone();
        let returned = accepted.pending.record.clone();
        let (observed_tx, observed_rx) = tokio::sync::oneshot::channel();
        accepted.ticket.observe(move |result| {
            let result = match result {
                Ok(_) => {
                    let mut state = state.lock().expect(LAUNCH_REPORT_STORE_LOCK_INVARIANT);
                    let PendingLaunchReport { revision, record } = accepted.pending;
                    let session_id = record.session_id.clone();
                    insert_committed_report(&mut state, record);
                    if state
                        .retry_candidate
                        .as_ref()
                        .is_some_and(|pending| pending.revision == revision)
                    {
                        state.retry_candidate = None;
                    }
                    state.writers.remove(&session_id);
                    Ok(())
                }
                Err(error) => {
                    state
                        .lock()
                        .expect(LAUNCH_REPORT_STORE_LOCK_INVARIANT)
                        .retry_candidate = Some(accepted.pending);
                    Err(io::Error::from(error))
                }
            };
            let _ = observed_tx.send((result, mutation));
        });
        let cleanup_state = self.state.clone();
        let cleanup_persistence = self.persistence.clone();
        let cleanup_retention = self.proof_retention.clone();
        let (completed_tx, completed_rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let Some((result, mutation)) = observed_rx.await.ok() else {
                return;
            };
            let settled = if result.is_ok() {
                reconcile_launch_report_cleanup(
                    cleanup_state,
                    cleanup_persistence,
                    cleanup_retention,
                    mutation,
                )
                .await
            } else {
                (result, mutation)
            };
            let _ = completed_tx.send(settled);
        });
        let (result, mutation) = completed_rx
            .await
            .map_err(|_| io::Error::other("launch report commit observer stopped"))?;
        result?;
        Ok((returned, mutation))
    }
}

#[cfg(test)]
impl Default for LaunchReportStore {
    fn default() -> Self {
        Self::in_memory()
    }
}

fn launch_report_target(session_id: &str) -> TargetDescriptor {
    TargetDescriptor::new(
        StabilizationSystem::State,
        TargetKind::Session,
        session_id,
        OwnershipClass::LauncherManaged,
    )
}

fn encode_launch_report(report: LaunchProofRecord) -> io::Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec_pretty(&report)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    bytes.push(b'\n');
    if bytes.len() as u64 > MAX_REPORT_BYTES {
        return Err(invalid_report("launch report is too large"));
    }
    Ok(bytes)
}

impl LaunchProofExport {
    fn from_record(record: &LaunchProofRecord) -> Self {
        Self {
            schema: sanitized_required_token(&record.schema, LAUNCH_PROOF_SCHEMA),
            schema_version: record.schema_version,
            session_id: sanitized_required_token(&record.session_id, "redacted"),
            instance_id: sanitized_required_token(&record.instance_id, "redacted"),
            version_id: sanitized_required_token(&record.version_id, "unknown"),
            launched_at: sanitized_required_token(&record.launched_at, "unknown"),
            recorded_at: sanitized_required_token(&record.recorded_at, "unknown"),
            outcome: sanitized_required_token(&record.outcome, "unknown"),
            session_outcome: record
                .session_outcome
                .as_ref()
                .map(|outcome| LaunchSessionOutcome::from_reason(outcome.reason)),
            scenario: sanitized_export_scenario(&record.scenario),
            device: sanitized_export_device(&record.device),
            resource_budget: record.resource_budget.clone(),
            pid: record.pid,
            exit_code: record.exit_code,
            boot_duration_ms: record.boot_duration_ms,
            failure_class: record
                .failure_class
                .as_deref()
                .and_then(sanitized_optional_token),
            guardian: record.guardian.as_ref().and_then(sanitized_guardian),
            healing: record.healing.as_ref().and_then(sanitized_healing),
            stages: record
                .stages
                .iter()
                .take(MAX_EXPORT_STAGES)
                .map(sanitized_stage)
                .collect(),
            comparison: record.comparison.as_ref().map(sanitized_comparison),
        }
    }
}

fn sanitized_export_scenario(scenario: &LaunchProofScenario) -> LaunchProofScenario {
    LaunchProofScenario {
        scenario_id: sanitized_required_token(&scenario.scenario_id, "unknown_launch"),
        performance_mode: sanitized_required_token(&scenario.performance_mode, "unknown"),
        requested_memory_mb: scenario.requested_memory_mb,
        version_id: scenario
            .version_id
            .as_deref()
            .and_then(sanitized_optional_token),
        benchmark_profile: scenario
            .benchmark_profile
            .as_deref()
            .and_then(sanitized_optional_token),
        benchmark_run_type: scenario
            .benchmark_run_type
            .as_deref()
            .and_then(sanitized_optional_token),
        benchmark_mode: scenario
            .benchmark_mode
            .as_deref()
            .and_then(sanitized_optional_token),
        benchmark_id: scenario
            .benchmark_id
            .as_deref()
            .and_then(sanitized_optional_token),
    }
}

fn sanitized_export_device(device: &LaunchProofDevice) -> LaunchProofDevice {
    LaunchProofDevice {
        tier: sanitized_required_token(&device.tier, "unknown"),
        total_memory_mb: device.total_memory_mb,
        cpu_threads: device.cpu_threads,
    }
}

fn sanitized_stage(stage: &LaunchStageRecord) -> LaunchProofStageExport {
    let stage = sanitized_stage_record(stage);
    LaunchProofStageExport {
        stage: stage.stage,
        label: stage.label,
        started_at_ms: stage.started_at_ms,
        ended_at_ms: stage.ended_at_ms,
        duration_ms: stage.duration_ms,
        result: stage.result,
        warnings: stage.warnings,
        fallback_reason: stage.fallback_reason,
        evidence: stage
            .evidence
            .iter()
            .filter_map(sanitized_stage_evidence)
            .take(MAX_EXPORT_DETAILS)
            .collect(),
    }
}

fn sanitized_stage_evidence(
    evidence: &LaunchStageEvidence,
) -> Option<LaunchProofStageEvidenceExport> {
    Some(LaunchProofStageEvidenceExport {
        id: sanitized_optional_token(&evidence.id)?,
        system: sanitized_optional_token(&evidence.system)?,
        summary: sanitized_bounded_text(&evidence.summary)?,
        details: evidence
            .details
            .iter()
            .filter_map(|detail| sanitized_bounded_text(detail))
            .take(MAX_EXPORT_DETAILS)
            .collect(),
    })
}

fn sanitized_guardian(value: &Value) -> Option<GuardianSummary> {
    let mut guardian = serde_json::from_value::<GuardianSummary>(value.clone()).ok()?;
    guardian.message = guardian.message.as_deref().and_then(sanitized_bounded_text);
    guardian.details = guardian
        .details
        .iter()
        .filter_map(|detail| sanitized_bounded_text(detail))
        .take(MAX_EXPORT_DETAILS)
        .collect();
    guardian.guidance = guardian
        .guidance
        .iter()
        .filter_map(|detail| sanitized_bounded_text(detail))
        .take(MAX_EXPORT_DETAILS)
        .collect();
    guardian.interventions = guardian
        .interventions
        .into_iter()
        .map(|mut intervention| {
            intervention.detail = intervention
                .detail
                .as_deref()
                .and_then(sanitized_bounded_text);
            intervention
        })
        .take(MAX_EXPORT_DETAILS)
        .collect();
    Some(guardian)
}

fn sanitized_healing(value: &Value) -> Option<LaunchHealingSummary> {
    let mut healing = serde_json::from_value::<LaunchHealingSummary>(value.clone()).ok()?;
    healing.requested_preset = healing
        .requested_preset
        .as_deref()
        .and_then(sanitized_optional_token);
    healing.effective_preset = healing
        .effective_preset
        .as_deref()
        .and_then(sanitized_optional_token);
    healing.auth_mode = healing
        .auth_mode
        .as_deref()
        .and_then(sanitized_optional_token);
    healing.failure_class = healing
        .failure_class
        .as_deref()
        .and_then(sanitized_optional_token);
    healing.warnings = healing
        .warnings
        .iter()
        .filter_map(|warning| sanitized_bounded_text(warning))
        .take(MAX_EXPORT_DETAILS)
        .collect();
    healing.fallback_applied = healing
        .fallback_applied
        .as_deref()
        .and_then(sanitized_bounded_text);
    healing.events = healing
        .events
        .into_iter()
        .map(|mut event| {
            event.detail = event.detail.as_deref().and_then(sanitized_bounded_text);
            event
        })
        .take(MAX_EXPORT_DETAILS)
        .collect();
    Some(healing)
}

fn sanitized_comparison(comparison: &LaunchProofComparison) -> LaunchProofComparison {
    LaunchProofComparison {
        baseline_session_id: sanitized_required_token(&comparison.baseline_session_id, "redacted"),
        baseline_recorded_at: sanitized_required_token(&comparison.baseline_recorded_at, "unknown"),
        baseline: LaunchProofComparisonBaseline {
            performance_mode: sanitized_required_token(
                &comparison.baseline.performance_mode,
                "unknown",
            ),
            version_id: sanitized_required_token(&comparison.baseline.version_id, "unknown"),
            requested_memory_mb: comparison.baseline.requested_memory_mb,
            device_tier: sanitized_required_token(&comparison.baseline.device_tier, "unknown"),
            benchmark_profile: comparison
                .baseline
                .benchmark_profile
                .as_deref()
                .and_then(sanitize_benchmark_metadata),
            benchmark_run_type: comparison
                .baseline
                .benchmark_run_type
                .as_deref()
                .and_then(sanitize_benchmark_metadata),
            benchmark_mode: comparison
                .baseline
                .benchmark_mode
                .as_deref()
                .and_then(sanitize_benchmark_mode_metadata),
        },
        matched_sample_count: comparison.matched_sample_count,
        metric_name: sanitized_required_token(&comparison.metric_name, "unknown"),
        current_value_ms: comparison.current_value_ms,
        baseline_value_ms: comparison.baseline_value_ms,
        delta_ms: comparison.delta_ms,
        delta_percent: comparison.delta_percent,
    }
}

fn sanitized_required_token(value: &str, fallback: &str) -> String {
    sanitized_optional_token(value).unwrap_or_else(|| fallback.to_string())
}

fn sanitized_optional_token(value: &str) -> Option<String> {
    sanitize_evidence_token(
        value,
        RedactionAudience::ExportableProof,
        MAX_EXPORT_TOKEN_CHARS,
    )
}

fn sanitized_bounded_text(value: &str) -> Option<String> {
    sanitize_evidence_text(
        value,
        RedactionAudience::ExportableProof,
        MAX_EXPORT_DETAIL_CHARS,
    )
}

fn build_record(
    record: &LaunchSessionRecord,
    launched_at: Option<&str>,
    outcome: &str,
    context: Option<&LaunchProofContext>,
) -> LaunchProofRecord {
    let recorded_at = timestamp_utc();
    let launched_at = launched_at
        .or(record.launched_at.as_deref())
        .filter(|value| !value.trim().is_empty())
        .unwrap_or(recorded_at.as_str())
        .to_string();
    let outcome = if outcome.trim().is_empty() {
        launch_state_name(record.state).to_string()
    } else {
        outcome.trim().to_ascii_lowercase()
    };

    LaunchProofRecord {
        schema: LAUNCH_PROOF_SCHEMA.to_string(),
        schema_version: LAUNCH_PROOF_SCHEMA_VERSION,
        session_id: record.session_id.0.clone(),
        instance_id: sanitized_required_token(&record.instance_id, "redacted"),
        version_id: sanitized_required_token(&record.version_id, "unknown"),
        launched_at,
        recorded_at,
        outcome,
        session_outcome: record.outcome.clone(),
        scenario: build_scenario(record, context),
        device: local_device_metadata(),
        resource_budget: context.and_then(|value| value.resource_budget.clone()),
        pid: record.pid,
        exit_code: record.exit_code,
        boot_duration_ms: record.boot_duration_ms,
        priority: record.priority.as_ref().map(sanitized_priority),
        failure_class: record
            .failure
            .as_ref()
            .and_then(|failure| sanitized_optional_token(failure.class.as_str())),
        failure_detail: record
            .failure
            .as_ref()
            .and_then(|failure| failure.detail.as_deref())
            .and_then(sanitized_bounded_text),
        guardian: record
            .guardian
            .as_ref()
            .and_then(sanitized_guardian)
            .and_then(|guardian| serde_json::to_value(guardian).ok()),
        healing: record
            .healing
            .as_ref()
            .and_then(sanitized_healing)
            .and_then(|healing| serde_json::to_value(healing).ok()),
        stages: record
            .stages
            .iter()
            .take(MAX_EXPORT_STAGES)
            .map(sanitized_stage_record)
            .collect(),
        comparison: None,
    }
}

fn sanitized_priority(priority: &LaunchPriorityEvidence) -> LaunchProofPriority {
    LaunchProofPriority {
        start_mode: sanitized_required_token(&priority.start_mode, "unknown"),
        start_error: priority
            .start_error
            .as_deref()
            .and_then(sanitized_bounded_text),
        promotion: priority
            .promotion
            .as_deref()
            .and_then(sanitized_optional_token),
        promotion_error: priority
            .promotion_error
            .as_deref()
            .and_then(sanitized_bounded_text),
    }
}

fn sanitized_stage_record(stage: &LaunchStageRecord) -> LaunchStageRecord {
    let stage_name = sanitized_required_token(&stage.stage, "unknown");
    LaunchStageRecord {
        stage: stage_name.clone(),
        label: sanitized_bounded_text(&stage.label).unwrap_or_else(|| stage_name.clone()),
        started_at_ms: stage.started_at_ms,
        ended_at_ms: stage.ended_at_ms,
        duration_ms: stage.duration_ms,
        result: stage.result.as_deref().and_then(sanitized_optional_token),
        warnings: stage
            .warnings
            .iter()
            .filter_map(|warning| sanitized_bounded_text(warning))
            .take(MAX_EXPORT_DETAILS)
            .collect(),
        fallback_reason: stage
            .fallback_reason
            .as_deref()
            .and_then(sanitized_bounded_text),
        evidence: stage
            .evidence
            .iter()
            .filter_map(sanitized_stage_evidence_record)
            .take(MAX_REPORT_STAGE_EVIDENCE)
            .collect(),
    }
}

fn sanitized_stage_evidence_record(evidence: &LaunchStageEvidence) -> Option<LaunchStageEvidence> {
    Some(LaunchStageEvidence {
        id: sanitized_optional_token(&evidence.id)?,
        system: sanitized_optional_token(&evidence.system)?,
        summary: sanitized_bounded_text(&evidence.summary)?,
        details: evidence
            .details
            .iter()
            .filter_map(|detail| sanitized_bounded_text(detail))
            .take(MAX_REPORT_EVIDENCE_DETAILS)
            .collect(),
    })
}

fn build_comparison_from_candidates(
    current: &LaunchProofRecord,
    candidates: &[LaunchProofRecord],
) -> Option<LaunchProofComparison> {
    if !launch_proof_outcome_is_comparable(&current.outcome) {
        return None;
    }

    let (metric_name, current_value_ms, metric_value) =
        launch_comparison_metric_for_current(current)?;
    let mut matches = candidates
        .iter()
        .filter(|candidate| report_precedes(candidate, current))
        .filter(|candidate| launch_proof_outcome_is_comparable(&candidate.outcome))
        .filter(|candidate| comparison_dimensions_match(current, candidate))
        .filter_map(|candidate| {
            let value_ms = metric_value(candidate)?;
            (value_ms > 0).then_some((candidate, value_ms))
        })
        .collect::<Vec<_>>();
    matches.sort_by(|(left, _), (right, _)| {
        comparison_baseline_mode_rank(current, left)
            .cmp(&comparison_baseline_mode_rank(current, right))
            .then_with(|| {
                right
                    .recorded_at
                    .cmp(&left.recorded_at)
                    .then_with(|| right.session_id.cmp(&left.session_id))
            })
    });
    let matched_sample_count = matches.len();
    let (baseline, baseline_value_ms) = matches.first()?;
    let delta_ms = metric_delta_ms(current_value_ms, *baseline_value_ms);

    Some(LaunchProofComparison {
        baseline_session_id: baseline.session_id.clone(),
        baseline_recorded_at: baseline.recorded_at.clone(),
        baseline: comparison_baseline_snapshot(baseline)?,
        matched_sample_count,
        metric_name: metric_name.to_string(),
        current_value_ms,
        baseline_value_ms: *baseline_value_ms,
        delta_ms,
        delta_percent: (delta_ms as f64 / *baseline_value_ms as f64) * 100.0,
    })
}

fn comparison_baseline_snapshot(
    report: &LaunchProofRecord,
) -> Option<LaunchProofComparisonBaseline> {
    Some(LaunchProofComparisonBaseline {
        performance_mode: known_launch_mode(report)?.to_string(),
        version_id: normalized_version_target(report)?.to_string(),
        requested_memory_mb: report.scenario.requested_memory_mb,
        device_tier: normalized_dimension(&report.device.tier)?.to_string(),
        benchmark_profile: report
            .scenario
            .benchmark_profile
            .as_deref()
            .and_then(normalized_dimension)
            .map(str::to_string),
        benchmark_run_type: report
            .scenario
            .benchmark_run_type
            .as_deref()
            .and_then(normalized_dimension)
            .map(str::to_string),
        benchmark_mode: report
            .scenario
            .benchmark_mode
            .as_deref()
            .and_then(normalized_dimension)
            .map(str::to_string),
    })
}

pub(crate) fn comparison_baseline_matches_report(
    comparison: &LaunchProofComparison,
    baseline: &LaunchProofRecord,
) -> bool {
    let metric_value = match comparison.metric_name.as_str() {
        LAUNCH_STAGE_COMPARISON_METRIC_NAME => launch_total_completed_stage_duration_ms(baseline),
        LAUNCH_BOOT_COMPARISON_METRIC_NAME => baseline.boot_duration_ms,
        _ => None,
    };
    launch_proof_outcome_is_comparable(&baseline.outcome)
        && comparison.baseline_session_id == baseline.session_id
        && comparison.baseline_recorded_at == baseline.recorded_at
        && comparison_baseline_snapshot(baseline).as_ref() == Some(&comparison.baseline)
        && metric_value == Some(comparison.baseline_value_ms)
}

fn comparison_baseline_mode_rank(current: &LaunchProofRecord, candidate: &LaunchProofRecord) -> u8 {
    match (known_launch_mode(current), known_launch_mode(candidate)) {
        (Some("managed"), Some("vanilla")) => 0,
        _ => 1,
    }
}

fn report_precedes(candidate: &LaunchProofRecord, current: &LaunchProofRecord) -> bool {
    (&candidate.recorded_at, &candidate.session_id) < (&current.recorded_at, &current.session_id)
}

fn launch_proof_outcome_is_comparable(outcome: &str) -> bool {
    matches!(outcome.trim(), "running" | "exited" | "completed")
}

fn launch_comparison_metric_for_current(
    current: &LaunchProofRecord,
) -> Option<LaunchComparisonMetric> {
    if let Some(boot_duration_ms) = current.boot_duration_ms {
        return Some((
            LAUNCH_BOOT_COMPARISON_METRIC_NAME,
            boot_duration_ms,
            launch_boot_duration_ms,
        ));
    }

    Some((
        LAUNCH_STAGE_COMPARISON_METRIC_NAME,
        launch_total_completed_stage_duration_ms(current)?,
        launch_total_completed_stage_duration_ms,
    ))
}

fn comparison_dimensions_match(current: &LaunchProofRecord, candidate: &LaunchProofRecord) -> bool {
    current.session_id != candidate.session_id
        && launch_modes_are_comparable(current, candidate)
        && required_version_targets_match(current, candidate)
        && current.scenario.requested_memory_mb == candidate.scenario.requested_memory_mb
        && required_dimensions_match(&current.device.tier, &candidate.device.tier)
        && optional_benchmark_dimensions_match(
            current.scenario.benchmark_profile.as_deref(),
            candidate.scenario.benchmark_profile.as_deref(),
        )
        && optional_benchmark_dimensions_match(
            current.scenario.benchmark_run_type.as_deref(),
            candidate.scenario.benchmark_run_type.as_deref(),
        )
        && optional_benchmark_dimensions_match(
            current.scenario.benchmark_mode.as_deref(),
            candidate.scenario.benchmark_mode.as_deref(),
        )
}

fn launch_modes_are_comparable(current: &LaunchProofRecord, candidate: &LaunchProofRecord) -> bool {
    matches!(
        (known_launch_mode(current), known_launch_mode(candidate)),
        (Some("managed"), Some("vanilla" | "managed"))
            | (Some("vanilla"), Some("vanilla"))
            | (Some("custom"), Some("custom"))
    )
}

fn known_launch_mode(report: &LaunchProofRecord) -> Option<&str> {
    let mode = normalized_dimension(&report.scenario.performance_mode)?;
    match mode {
        "managed" | "vanilla" | "custom"
            if required_dimensions_match(
                &report.scenario.scenario_id,
                scenario_id_for_performance_mode(mode),
            ) =>
        {
            Some(mode)
        }
        _ => None,
    }
}

fn required_dimensions_match(left: &str, right: &str) -> bool {
    match (normalized_dimension(left), normalized_dimension(right)) {
        (Some(left), Some(right)) => left == right,
        _ => false,
    }
}

fn optional_benchmark_dimensions_match(left: Option<&str>, right: Option<&str>) -> bool {
    match (
        left.and_then(normalized_dimension),
        right.and_then(normalized_dimension),
    ) {
        (Some(left), Some(right)) => left == right,
        (None, None) => true,
        _ => false,
    }
}

fn required_version_targets_match(
    current: &LaunchProofRecord,
    candidate: &LaunchProofRecord,
) -> bool {
    match (
        normalized_version_target(current),
        normalized_version_target(candidate),
    ) {
        (Some(current), Some(candidate)) => current == candidate,
        _ => false,
    }
}

fn normalized_dimension(value: &str) -> Option<&str> {
    let value = value.trim();
    if value.is_empty() || value == "unknown" {
        None
    } else {
        Some(value)
    }
}

fn normalized_version_target(report: &LaunchProofRecord) -> Option<&str> {
    report
        .scenario
        .version_id
        .as_deref()
        .and_then(normalized_dimension)
        .or_else(|| normalized_dimension(&report.version_id))
}

// Metric source: launch stage history. The value is the sum of completed stage
// durations, using duration_ms when present and falling back to ended-started.
fn launch_total_completed_stage_duration_ms(report: &LaunchProofRecord) -> Option<u64> {
    let mut total = 0_u64;
    let mut completed = false;
    for stage in &report.stages {
        let Some(ended_at_ms) = stage.ended_at_ms else {
            continue;
        };
        let duration_ms = stage
            .duration_ms
            .unwrap_or_else(|| ended_at_ms.saturating_sub(stage.started_at_ms));
        total = total.saturating_add(duration_ms);
        completed = true;
    }
    completed.then_some(total)
}

fn launch_boot_duration_ms(report: &LaunchProofRecord) -> Option<u64> {
    report.boot_duration_ms
}

fn metric_delta_ms(current_value_ms: u64, baseline_value_ms: u64) -> i64 {
    if current_value_ms >= baseline_value_ms {
        i64::try_from(current_value_ms - baseline_value_ms).unwrap_or(i64::MAX)
    } else {
        -i64::try_from(baseline_value_ms - current_value_ms).unwrap_or(i64::MAX)
    }
}

fn build_scenario(
    record: &LaunchSessionRecord,
    context: Option<&LaunchProofContext>,
) -> LaunchProofScenario {
    let performance_mode = context
        .map(|value| trimmed_or_unknown(&value.performance_mode))
        .unwrap_or_else(|| "unknown".to_string());
    let version_id = context
        .and_then(|value| value.version_id.clone())
        .or_else(|| non_empty_string(&record.version_id))
        .and_then(|value| sanitized_optional_token(&value));
    let benchmark = context.and_then(|value| value.benchmark.as_ref());

    LaunchProofScenario {
        scenario_id: scenario_id_for_performance_mode(&performance_mode).to_string(),
        performance_mode,
        requested_memory_mb: context.and_then(|value| value.requested_memory_mb),
        version_id,
        benchmark_profile: benchmark.and_then(|value| value.profile.clone()),
        benchmark_run_type: benchmark.and_then(|value| value.run_type.clone()),
        benchmark_mode: benchmark.and_then(|value| value.mode.clone()),
        benchmark_id: benchmark.and_then(|value| value.benchmark_id.clone()),
    }
}

fn scenario_id_for_performance_mode(performance_mode: &str) -> &'static str {
    match performance_mode.trim() {
        "managed" => "managed_launch",
        "vanilla" => "vanilla_launch",
        "custom" => "custom_launch",
        _ => "unknown_launch",
    }
}

fn local_device_metadata() -> LaunchProofDevice {
    let total_memory_mb = host_total_memory_mb();
    let cpu_threads = std::thread::available_parallelism().ok().map(usize::from);

    LaunchProofDevice {
        tier: classify_device_tier(cpu_threads, total_memory_mb).to_string(),
        total_memory_mb,
        cpu_threads,
    }
}

fn host_total_memory_mb() -> Option<u64> {
    let mut system = System::new();
    system.refresh_memory();
    let total_memory_mb = system.total_memory() / (1024 * 1024);
    (total_memory_mb > 0).then_some(total_memory_mb)
}

fn classify_device_tier(cpu_threads: Option<usize>, total_memory_mb: Option<u64>) -> &'static str {
    let mut tiers = Vec::new();
    if let Some(cpu_threads) = cpu_threads.filter(|value| *value > 0) {
        tiers.push(if cpu_threads <= 4 {
            DeviceTier::Low
        } else if cpu_threads >= 8 {
            DeviceTier::High
        } else {
            DeviceTier::Mid
        });
    }
    if let Some(total_memory_mb) = total_memory_mb.filter(|value| *value > 0) {
        tiers.push(if total_memory_mb <= 8_192 {
            DeviceTier::Low
        } else if total_memory_mb >= 32_768 {
            DeviceTier::High
        } else {
            DeviceTier::Mid
        });
    }

    match tiers.into_iter().min() {
        Some(DeviceTier::Low) => "low",
        Some(DeviceTier::Mid) => "mid",
        Some(DeviceTier::High) => "high",
        None => "unknown",
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum DeviceTier {
    Low,
    Mid,
    High,
}

fn trimmed_or_unknown(value: &str) -> String {
    let value = value.trim();
    if value.is_empty() {
        "unknown".to_string()
    } else {
        value.to_string()
    }
}

fn non_empty_string(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_string())
}

fn sanitize_benchmark_metadata(value: &str) -> Option<String> {
    sanitize_evidence_token(
        value,
        RedactionAudience::ExportableProof,
        MAX_BENCHMARK_METADATA_CHARS,
    )
}

fn sanitize_benchmark_mode_metadata(value: &str) -> Option<String> {
    let sanitized = sanitize_benchmark_metadata(value)?;
    let normalized = match sanitized.as_str() {
        "development" => "development",
        "qualification" => "qualification",
        "release_validation" => "release_validation",
        _ => return None,
    };
    Some(normalized.to_string())
}

fn positive_i32(value: i32) -> Option<i32> {
    (value > 0).then_some(value)
}

fn report_dir(paths: &AppPaths) -> PathBuf {
    paths.config_dir.join("benchmarks").join("launch")
}

fn report_path_in(directory: &Path, session_id: &str) -> PathBuf {
    directory.join(report_filename(session_id))
}

#[cfg(test)]
pub(crate) fn report_path(paths: &AppPaths, session_id: &str) -> PathBuf {
    report_path_in(&report_dir(paths), session_id)
}

fn load_report_index(
    directory: &Path,
    protected_session_ids: &HashSet<String>,
) -> (BTreeMap<String, LaunchProofRecord>, usize) {
    match fs::symlink_metadata(directory) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            return (BTreeMap::new(), 1);
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => return (BTreeMap::new(), 0),
        Err(_) => return (BTreeMap::new(), 1),
    }
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(_) => return (BTreeMap::new(), 1),
    };
    let mut paths = BTreeSet::new();
    let mut issues = 0usize;
    for session_id in protected_session_ids {
        if !canonical_session_id(session_id) {
            issues = bounded_issue_count(issues);
            continue;
        }
        let path = report_path_in(directory, session_id);
        match fs::symlink_metadata(&path) {
            Ok(_) => {
                paths.insert(path);
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(_) => issues = bounded_issue_count(issues),
        }
    }
    for entry in entries.take(MAX_STARTUP_REPORTS.saturating_add(1)) {
        let Ok(entry) = entry else {
            issues = bounded_issue_count(issues);
            continue;
        };
        let path = entry.path();
        if paths.contains(&path) {
            continue;
        }
        if paths.len() == MAX_STARTUP_REPORTS {
            issues = bounded_issue_count(issues);
            break;
        }
        paths.insert(path);
    }

    let mut reports = BTreeMap::new();
    for path in paths {
        match load_admitted_report(&path) {
            Ok(report) => {
                reports.insert(report.session_id.clone(), report);
            }
            Err(_) => issues = bounded_issue_count(issues),
        }
    }
    let mut ordinary = reports
        .values()
        .filter(|report| !protected_session_ids.contains(&report.session_id))
        .cloned()
        .collect::<Vec<_>>();
    sort_reports(&mut ordinary);
    let protected_report_count = reports
        .keys()
        .filter(|session_id| protected_session_ids.contains(*session_id))
        .count();
    let ordinary_limit = MAX_STARTUP_REPORTS.saturating_sub(protected_report_count);
    for report in ordinary.into_iter().skip(ordinary_limit) {
        reports.remove(&report.session_id);
        issues = bounded_issue_count(issues);
    }
    (reports, issues)
}

fn bounded_issue_count(current: usize) -> usize {
    current.saturating_add(1).min(MAX_LOAD_ISSUES)
}

fn load_admitted_report(path: &Path) -> io::Result<LaunchProofRecord> {
    let file_name = path
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| invalid_report("launch report filename is not UTF-8"))?;
    if path.extension().and_then(|value| value.to_str()) != Some("json") {
        return Err(invalid_report("launch report filename is not canonical"));
    }
    let (metadata_before, identity_before) = admitted_path_snapshot(path)?;
    if metadata_before.file_type().is_symlink()
        || !metadata_before.is_file()
        || metadata_before.len() > MAX_REPORT_BYTES
    {
        return Err(invalid_report("launch report file is not admissible"));
    }
    let file = File::open(path)?;
    let opened_metadata = file.metadata()?;
    let opened_identity = admitted_file_identity(&file, &opened_metadata)?;
    let (metadata_after, identity_after) = admitted_path_snapshot(path)?;
    if metadata_after.file_type().is_symlink()
        || !metadata_after.is_file()
        || opened_metadata.len() > MAX_REPORT_BYTES
    {
        return Err(invalid_report(
            "launch report file identity changed during admission",
        ));
    }
    if identity_before != opened_identity || opened_identity != identity_after {
        return Err(invalid_report(
            "launch report file identity changed during admission",
        ));
    }
    let capacity = usize::try_from(opened_metadata.len())
        .map_err(|_| invalid_report("launch report file is too large"))?;
    let mut bytes = Vec::with_capacity(capacity);
    file.take(MAX_REPORT_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_REPORT_BYTES {
        return Err(invalid_report("launch report file is too large"));
    }
    let report: LaunchProofRecord = serde_json::from_slice(&bytes)
        .map_err(|_| invalid_report("launch report schema is malformed"))?;
    validate_admitted_report(&report, file_name)?;
    Ok(report)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AdmittedFileIdentity {
    #[cfg(unix)]
    Unix { device: u64, inode: u64 },
    #[cfg(windows)]
    Windows {
        volume_serial: u64,
        file_id: [u8; 16],
    },
}

#[cfg(unix)]
fn admitted_path_snapshot(path: &Path) -> io::Result<(fs::Metadata, AdmittedFileIdentity)> {
    let metadata = fs::symlink_metadata(path)?;
    let identity = admitted_unix_identity(&metadata)?;
    Ok((metadata, identity))
}

#[cfg(unix)]
fn admitted_unix_identity(metadata: &fs::Metadata) -> io::Result<AdmittedFileIdentity> {
    use std::os::unix::fs::MetadataExt;

    if !metadata.file_type().is_file() {
        return Err(invalid_report("launch report identity is not a file"));
    }
    Ok(AdmittedFileIdentity::Unix {
        device: metadata.dev(),
        inode: metadata.ino(),
    })
}

#[cfg(windows)]
fn admitted_path_snapshot(path: &Path) -> io::Result<(fs::Metadata, AdmittedFileIdentity)> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    };

    let file = fs::OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    let metadata = file.metadata()?;
    let identity = admitted_file_identity(&file, &metadata)?;
    Ok((metadata, identity))
}

#[cfg(not(any(unix, windows)))]
fn admitted_path_snapshot(_path: &Path) -> io::Result<(fs::Metadata, AdmittedFileIdentity)> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "exact launch report identity is unavailable on this platform",
    ))
}

#[cfg(unix)]
fn admitted_file_identity(
    _file: &File,
    metadata: &fs::Metadata,
) -> io::Result<AdmittedFileIdentity> {
    admitted_unix_identity(metadata)
}

#[cfg(windows)]
fn admitted_file_identity(
    file: &File,
    metadata: &fs::Metadata,
) -> io::Result<AdmittedFileIdentity> {
    use std::mem::size_of;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ID_INFO, FileIdInfo, GetFileInformationByHandleEx,
    };

    if !metadata.file_type().is_file() {
        return Err(invalid_report("launch report identity is not a file"));
    }
    let mut info = FILE_ID_INFO::default();
    // SAFETY: `file` owns a valid handle, and `info` is a correctly sized writable buffer.
    let succeeded = unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle() as HANDLE,
            FileIdInfo,
            (&raw mut info).cast(),
            size_of::<FILE_ID_INFO>() as u32,
        )
    };
    if succeeded == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(AdmittedFileIdentity::Windows {
        volume_serial: info.VolumeSerialNumber,
        file_id: info.FileId.Identifier,
    })
}

#[cfg(not(any(unix, windows)))]
fn admitted_file_identity(
    _file: &File,
    _metadata: &fs::Metadata,
) -> io::Result<AdmittedFileIdentity> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "exact launch report identity is unavailable on this platform",
    ))
}

fn validate_admitted_report(report: &LaunchProofRecord, file_name: &str) -> io::Result<()> {
    if report.schema != LAUNCH_PROOF_SCHEMA
        || report.schema_version != LAUNCH_PROOF_SCHEMA_VERSION
        || report_filename(&report.session_id) != file_name
        || !canonical_session_id(&report.session_id)
        || sanitized_optional_token(&report.instance_id).as_deref()
            != Some(report.instance_id.as_str())
        || sanitized_optional_token(&report.version_id).as_deref()
            != Some(report.version_id.as_str())
        || !known_report_outcome(&report.outcome)
        || !optional_session_outcome_is_canonical(report.session_outcome.as_ref())
        || !session_outcome_matches_report_outcome(&report.outcome, report.session_outcome.as_ref())
        || !canonical_report_timestamp(&report.launched_at)
        || !canonical_report_timestamp(&report.recorded_at)
        || report.recorded_at < report.launched_at
        || report.scenario.scenario_id
            != scenario_id_for_performance_mode(&report.scenario.performance_mode)
        || !report_scenario_is_canonical(&report.scenario)
        || !matches!(
            report.device.tier.as_str(),
            "low" | "mid" | "high" | "unknown"
        )
        || report.device.total_memory_mb == Some(0)
        || report.device.cpu_threads == Some(0)
        || report.stages.len() > MAX_EXPORT_STAGES
        || report.stages.iter().any(|stage| {
            sanitized_stage_record(stage) != *stage || !stage_timing_is_coherent(stage)
        })
        || !optional_bounded_text_is_canonical(report.failure_detail.as_deref())
        || !optional_token_is_canonical(report.failure_class.as_deref())
        || !optional_priority_is_canonical(report.priority.as_ref())
        || !optional_guardian_is_canonical(report.guardian.as_ref())
        || !optional_healing_is_canonical(report.healing.as_ref())
        || report
            .comparison
            .as_ref()
            .is_some_and(|comparison| !comparison_is_coherent(report, comparison))
    {
        return Err(invalid_report("launch report semantics are not current"));
    }
    Ok(())
}

fn comparison_is_coherent(report: &LaunchProofRecord, comparison: &LaunchProofComparison) -> bool {
    let expected_percent =
        (comparison.delta_ms as f64 / comparison.baseline_value_ms as f64) * 100.0;
    comparison.baseline_session_id != report.session_id
        && canonical_session_id(&comparison.baseline_session_id)
        && canonical_report_timestamp(&comparison.baseline_recorded_at)
        && (
            &comparison.baseline_recorded_at,
            &comparison.baseline_session_id,
        ) < (&report.recorded_at, &report.session_id)
        && comparison_baseline_is_compatible(report, &comparison.baseline)
        && comparison.matched_sample_count > 0
        && comparison.matched_sample_count <= MAX_STARTUP_REPORTS
        && comparison.current_value_ms > 0
        && comparison.baseline_value_ms > 0
        && comparison.delta_ms
            == metric_delta_ms(comparison.current_value_ms, comparison.baseline_value_ms)
        && comparison.delta_percent.is_finite()
        && (comparison.delta_percent - expected_percent).abs()
            <= f64::EPSILON * expected_percent.abs().max(1.0) * 4.0
        && matches!(
            comparison.metric_name.as_str(),
            LAUNCH_STAGE_COMPARISON_METRIC_NAME | LAUNCH_BOOT_COMPARISON_METRIC_NAME
        )
        && match comparison.metric_name.as_str() {
            LAUNCH_STAGE_COMPARISON_METRIC_NAME => {
                launch_total_completed_stage_duration_ms(report)
                    == Some(comparison.current_value_ms)
            }
            LAUNCH_BOOT_COMPARISON_METRIC_NAME => {
                report.boot_duration_ms == Some(comparison.current_value_ms)
            }
            _ => false,
        }
}

fn comparison_baseline_is_compatible(
    current: &LaunchProofRecord,
    baseline: &LaunchProofComparisonBaseline,
) -> bool {
    matches!(
        (
            known_launch_mode(current),
            baseline.performance_mode.as_str()
        ),
        (Some("managed"), "vanilla" | "managed")
            | (Some("vanilla"), "vanilla")
            | (Some("custom"), "custom")
    ) && normalized_version_target(current) == Some(baseline.version_id.as_str())
        && current.scenario.requested_memory_mb == baseline.requested_memory_mb
        && normalized_dimension(&current.device.tier) == Some(baseline.device_tier.as_str())
        && optional_benchmark_dimensions_match(
            current.scenario.benchmark_profile.as_deref(),
            baseline.benchmark_profile.as_deref(),
        )
        && optional_benchmark_dimensions_match(
            current.scenario.benchmark_run_type.as_deref(),
            baseline.benchmark_run_type.as_deref(),
        )
        && optional_benchmark_dimensions_match(
            current.scenario.benchmark_mode.as_deref(),
            baseline.benchmark_mode.as_deref(),
        )
        && sanitized_optional_token(&baseline.performance_mode).as_deref()
            == Some(baseline.performance_mode.as_str())
        && sanitized_optional_token(&baseline.version_id).as_deref()
            == Some(baseline.version_id.as_str())
        && sanitized_optional_token(&baseline.device_tier).as_deref()
            == Some(baseline.device_tier.as_str())
        && optional_benchmark_value_is_canonical(baseline.benchmark_profile.as_deref())
        && optional_benchmark_value_is_canonical(baseline.benchmark_run_type.as_deref())
        && baseline.benchmark_mode.as_deref().is_none_or(|value| {
            matches!(
                value,
                "development" | "qualification" | "release_validation"
            )
        })
}

fn known_report_outcome(outcome: &str) -> bool {
    matches!(
        outcome,
        "running"
            | "degraded"
            | "failed"
            | "exited"
            | "completed"
            | "stopped"
            | "cancelled"
            | "canceled"
            | "unknown"
    )
}

fn canonical_report_timestamp(value: &str) -> bool {
    chrono::DateTime::parse_from_rfc3339(value).is_ok_and(|timestamp| {
        timestamp
            .with_timezone(&chrono::Utc)
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
            == value
    })
}

async fn reconcile_launch_report_cleanup(
    state: Arc<Mutex<LaunchReportState>>,
    persistence: Option<Arc<LaunchReportPersistence>>,
    proof_retention: Arc<RwLock<BenchmarkProofRetentionHandle>>,
    mutation: OwnedMutexGuard<()>,
) -> (io::Result<()>, OwnedMutexGuard<()>) {
    loop {
        let retention = proof_retention
            .read()
            .expect(LAUNCH_REPORT_STORE_LOCK_INVARIANT)
            .clone();
        let Some(protected_session_ids) = retention.retained_session_ids(MAX_STARTUP_REPORTS)
        else {
            return (
                Err(invalid_report(
                    "launch report retention claims exceed the current bound",
                )),
                mutation,
            );
        };
        let selection_state = state.clone();
        let selected = tokio::task::spawn_blocking(move || {
            select_report_cleanup_candidate(&selection_state, &protected_session_ids)
        })
        .await;
        let session_id = match selected {
            Ok(Some(session_id)) => session_id,
            Ok(None) => return (Ok(()), mutation),
            Err(_) => {
                return (
                    Err(io::Error::other(
                        "launch report retention selection task stopped",
                    )),
                    mutation,
                );
            }
        };
        let Some(_proof_prune) = retention.try_begin_prune(&session_id).await else {
            let mut report_state = state.lock().expect(LAUNCH_REPORT_STORE_LOCK_INVARIANT);
            if report_state.cleanup_candidate.as_deref() == Some(session_id.as_str()) {
                report_state.cleanup_candidate = None;
            }
            continue;
        };

        if let Some(persistence) = &persistence {
            let path = report_path_in(&persistence.directory, &session_id);
            let delete_session_id = session_id.clone();
            let deletion = tokio::task::spawn_blocking(move || {
                delete_launcher_managed_file(DeleteFileRequest::new(
                    launch_report_target(&delete_session_id),
                    &path,
                ))
            })
            .await;
            match deletion {
                Ok(Ok(_)) => {}
                Ok(Err(error)) => {
                    return (Err(io::Error::new(error.io_kind(), error)), mutation);
                }
                Err(_) => {
                    return (
                        Err(io::Error::other(
                            "launch report retention deletion task stopped",
                        )),
                        mutation,
                    );
                }
            }
        }

        let mut report_state = state.lock().expect(LAUNCH_REPORT_STORE_LOCK_INVARIANT);
        if report_state.cleanup_candidate.as_deref() == Some(session_id.as_str()) {
            remove_committed_report(&mut report_state, &session_id);
            report_state.cleanup_candidate = None;
        }
    }
}

fn select_report_cleanup_candidate(
    state: &Arc<Mutex<LaunchReportState>>,
    protected_session_ids: &HashSet<String>,
) -> Option<String> {
    let mut state = state.lock().expect(LAUNCH_REPORT_STORE_LOCK_INVARIANT);
    let protected_report_count = state
        .reports
        .keys()
        .filter(|session_id| protected_session_ids.contains(*session_id))
        .count();
    let ordinary_limit = MAX_STARTUP_REPORTS.saturating_sub(protected_report_count);
    let ordinary_count = state
        .reports
        .keys()
        .filter(|session_id| !protected_session_ids.contains(*session_id))
        .count();
    if ordinary_count <= ordinary_limit {
        state.cleanup_candidate = None;
        return None;
    }
    if let Some(session_id) = state.cleanup_candidate.as_ref()
        && state.reports.contains_key(session_id)
        && !protected_session_ids.contains(session_id)
    {
        return Some(session_id.clone());
    }
    let selected = state.order.iter().find_map(|(_, session_id)| {
        (!protected_session_ids.contains(session_id)).then(|| session_id.clone())
    });
    state.cleanup_candidate = selected.clone();
    selected
}

fn report_scenario_is_canonical(scenario: &LaunchProofScenario) -> bool {
    matches!(
        scenario.performance_mode.as_str(),
        "managed" | "vanilla" | "custom" | "unknown"
    ) && scenario.requested_memory_mb.is_none_or(|value| value > 0)
        && optional_token_is_canonical(scenario.version_id.as_deref())
        && optional_benchmark_value_is_canonical(scenario.benchmark_profile.as_deref())
        && optional_benchmark_value_is_canonical(scenario.benchmark_run_type.as_deref())
        && optional_benchmark_value_is_canonical(scenario.benchmark_id.as_deref())
        && scenario.benchmark_mode.as_deref().is_none_or(|value| {
            matches!(
                value,
                "development" | "qualification" | "release_validation"
            )
        })
}

fn optional_benchmark_value_is_canonical(value: Option<&str>) -> bool {
    value.is_none_or(|value| sanitize_benchmark_metadata(value).as_deref() == Some(value))
}

fn optional_token_is_canonical(value: Option<&str>) -> bool {
    value.is_none_or(|value| sanitized_optional_token(value).as_deref() == Some(value))
}

fn optional_bounded_text_is_canonical(value: Option<&str>) -> bool {
    value.is_none_or(|value| sanitized_bounded_text(value).as_deref() == Some(value))
}

fn optional_session_outcome_is_canonical(outcome: Option<&LaunchSessionOutcome>) -> bool {
    outcome.is_none_or(|outcome| LaunchSessionOutcome::from_reason(outcome.reason) == *outcome)
}

fn session_outcome_matches_report_outcome(
    report_outcome: &str,
    session_outcome: Option<&LaunchSessionOutcome>,
) -> bool {
    let Some(session_outcome) = session_outcome else {
        return true;
    };
    match report_outcome {
        "running" | "degraded" => false,
        "failed" => matches!(
            session_outcome.kind,
            LaunchSessionOutcomeKind::Failed | LaunchSessionOutcomeKind::Unknown
        ),
        "exited" => true,
        "completed" => matches!(session_outcome.kind, LaunchSessionOutcomeKind::Clean),
        "stopped" | "cancelled" | "canceled" => {
            matches!(session_outcome.kind, LaunchSessionOutcomeKind::Stopped)
        }
        "unknown" => matches!(session_outcome.kind, LaunchSessionOutcomeKind::Unknown),
        _ => false,
    }
}

fn optional_priority_is_canonical(priority: Option<&LaunchProofPriority>) -> bool {
    priority.is_none_or(|priority| {
        sanitized_optional_token(&priority.start_mode).as_deref()
            == Some(priority.start_mode.as_str())
            && optional_bounded_text_is_canonical(priority.start_error.as_deref())
            && optional_token_is_canonical(priority.promotion.as_deref())
            && optional_bounded_text_is_canonical(priority.promotion_error.as_deref())
    })
}

fn optional_guardian_is_canonical(guardian: Option<&Value>) -> bool {
    guardian.is_none_or(|guardian| {
        sanitized_guardian(guardian)
            .and_then(|guardian| serde_json::to_value(guardian).ok())
            .as_ref()
            == Some(guardian)
    })
}

fn optional_healing_is_canonical(healing: Option<&Value>) -> bool {
    healing.is_none_or(|healing| {
        sanitized_healing(healing)
            .and_then(|healing| serde_json::to_value(healing).ok())
            .as_ref()
            == Some(healing)
    })
}

fn stage_timing_is_coherent(stage: &LaunchStageRecord) -> bool {
    match (stage.ended_at_ms, stage.duration_ms) {
        (Some(ended_at), Some(duration)) => {
            ended_at >= stage.started_at_ms
                && ended_at.saturating_sub(stage.started_at_ms) == duration
        }
        (None, None) => true,
        _ => false,
    }
}

fn canonical_session_id(session_id: &str) -> bool {
    !session_id.is_empty()
        && session_id.len() <= MAX_REPORT_FILENAME_STEM
        && session_id.bytes().all(|value| {
            value.is_ascii_lowercase() || value.is_ascii_digit() || matches!(value, b'-' | b'_')
        })
}

fn invalid_report(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn sort_reports(reports: &mut [LaunchProofRecord]) {
    reports.sort_by(|left, right| {
        right
            .recorded_at
            .cmp(&left.recorded_at)
            .then_with(|| right.session_id.cmp(&left.session_id))
    });
}

fn report_order(reports: &BTreeMap<String, LaunchProofRecord>) -> BTreeSet<(String, String)> {
    reports
        .values()
        .map(|report| (report.recorded_at.clone(), report.session_id.clone()))
        .collect()
}

fn insert_committed_report(state: &mut LaunchReportState, report: LaunchProofRecord) {
    if let Some(previous) = state
        .reports
        .insert(report.session_id.clone(), report.clone())
    {
        state
            .order
            .remove(&(previous.recorded_at, previous.session_id));
    }
    state.order.insert((report.recorded_at, report.session_id));
}

fn remove_committed_report(state: &mut LaunchReportState, session_id: &str) {
    if let Some(report) = state.reports.remove(session_id) {
        state.order.remove(&(report.recorded_at, report.session_id));
    }
}

fn list_recent_from_state(state: &LaunchReportState, limit: usize) -> Vec<LaunchProofRecord> {
    state
        .order
        .iter()
        .rev()
        .take(limit)
        .filter_map(|(_, session_id)| state.reports.get(session_id).cloned())
        .collect()
}

fn report_is_terminal(outcome: &str) -> bool {
    matches!(
        outcome.trim(),
        "failed" | "exited" | "completed" | "stopped" | "cancelled" | "canceled"
    )
}

fn report_filename(session_id: &str) -> String {
    format!("{session_id}.json")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execution::persistence::AtomicWriteBackend;
    use axial_config::AppPaths;
    use axial_launcher::service::HealingSummaryInput;
    use axial_launcher::{
        LaunchFailure, LaunchFailureClass, LaunchSessionExitReason, LaunchState, SessionId,
        build_healing_summary, launch_stage_label,
    };
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Condvar};
    use std::time::{SystemTime, UNIX_EPOCH};
    use tokio::sync::Notify;

    struct RecordingBackend {
        attempts: AtomicUsize,
        failures: AtomicUsize,
        committed: Mutex<Vec<Vec<u8>>>,
        started: Notify,
        gate: Mutex<Option<Arc<WriteGate>>>,
    }

    struct WriteGate {
        released: Mutex<bool>,
        changed: Condvar,
    }

    struct WriteGateHandle(Arc<WriteGate>);

    impl RecordingBackend {
        fn new() -> Self {
            Self {
                attempts: AtomicUsize::new(0),
                failures: AtomicUsize::new(0),
                committed: Mutex::new(Vec::new()),
                started: Notify::new(),
                gate: Mutex::new(None),
            }
        }

        fn fail_next(&self) {
            self.failures.fetch_add(1, Ordering::SeqCst);
        }

        fn gate_next(&self) -> WriteGateHandle {
            let gate = Arc::new(WriteGate {
                released: Mutex::new(false),
                changed: Condvar::new(),
            });
            *self.gate.lock().expect("backend gate lock") = Some(gate.clone());
            WriteGateHandle(gate)
        }

        async fn wait_for_attempt(&self, expected: usize) {
            loop {
                let started = self.started.notified();
                if self.attempts.load(Ordering::SeqCst) >= expected {
                    return;
                }
                started.await;
            }
        }

        fn committed_reports(&self) -> Vec<LaunchProofRecord> {
            self.committed
                .lock()
                .expect("committed report lock")
                .iter()
                .map(|contents| {
                    serde_json::from_slice(contents).expect("decode committed launch report")
                })
                .collect()
        }
    }

    impl AtomicWriteBackend for RecordingBackend {
        fn write(
            &self,
            _target: &TargetDescriptor,
            _destination: &Path,
            contents: &[u8],
        ) -> io::Result<()> {
            self.attempts.fetch_add(1, Ordering::SeqCst);
            self.started.notify_one();
            if let Some(gate) = self.gate.lock().expect("backend gate lock").take() {
                gate.wait();
            }
            if self
                .failures
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |failures| {
                    (failures > 0).then(|| failures - 1)
                })
                .is_ok()
            {
                return Err(io::Error::other("injected launch report write failure"));
            }
            self.committed
                .lock()
                .expect("committed report lock")
                .push(contents.to_vec());
            Ok(())
        }
    }

    impl WriteGate {
        fn release(&self) {
            *self.released.lock().expect("write gate lock") = true;
            self.changed.notify_all();
        }

        fn wait(&self) {
            let mut released = self.released.lock().expect("write gate lock");
            while !*released {
                released = self.changed.wait(released).expect("wait on write gate");
            }
        }
    }

    impl WriteGateHandle {
        fn release(&self) {
            self.0.release();
        }
    }

    impl Drop for WriteGateHandle {
        fn drop(&mut self) {
            self.0.release();
        }
    }

    fn persist_test_report(
        paths: &AppPaths,
        record: &LaunchSessionRecord,
        launched_at: Option<&str>,
        outcome: &str,
    ) -> io::Result<LaunchProofRecord> {
        persist_test_report_with_context(paths, record, launched_at, outcome, None)
    }

    fn persist_test_report_with_context(
        paths: &AppPaths,
        record: &LaunchSessionRecord,
        launched_at: Option<&str>,
        outcome: &str,
        context: Option<&LaunchProofContext>,
    ) -> io::Result<LaunchProofRecord> {
        let store = LaunchReportStore::load_from_paths_for_test(paths);
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build launch report test runtime")
            .block_on(store.persist(
                record.clone(),
                launched_at.map(str::to_string),
                outcome.to_string(),
                context.cloned(),
            ))
    }

    fn load_test_report(
        paths: &AppPaths,
        session_id: &str,
    ) -> io::Result<Option<LaunchProofRecord>> {
        match load_admitted_report(&report_path(paths, session_id)) {
            Ok(report) => Ok(Some(report)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    fn list_test_reports(paths: &AppPaths, limit: usize) -> io::Result<Vec<LaunchProofRecord>> {
        let (reports, issues) = load_report_index(&report_dir(paths), &HashSet::new());
        if issues != 0 {
            return Err(invalid_report(
                "launch report index contains rejected input",
            ));
        }
        let mut reports = reports.into_values().collect::<Vec<_>>();
        sort_reports(&mut reports);
        reports.truncate(limit);
        Ok(reports)
    }

    #[test]
    fn launch_report_path_requires_canonical_session_id() {
        let root = test_root("safe-path");
        let paths = test_paths(&root);

        let path = report_path(&paths, "canonical-session_1");

        assert_eq!(path.parent(), Some(report_dir(&paths).as_path()));
        assert_eq!(
            path.file_name().and_then(|value| value.to_str()),
            Some("canonical-session_1.json")
        );
        assert!(path.starts_with(paths.config_dir.join("benchmarks").join("launch")));
        assert!(!canonical_session_id("../bad/session\\id:?"));

        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn cancelled_report_commit_stays_hidden_then_publishes_and_releases_writer() {
        let (root, backend, store) = persistence_fixture("cancelled-commit");
        let store = Arc::new(store);
        let gate = backend.gate_next();
        let task_store = store.clone();
        let task = tokio::spawn(async move {
            task_store
                .persist(
                    test_record("cancelled-commit"),
                    None,
                    "running".to_string(),
                    None,
                )
                .await
        });

        backend.wait_for_attempt(1).await;
        assert!(store.load("cancelled-commit").is_none());
        task.abort();
        assert!(task.await.expect_err("caller cancelled").is_cancelled());
        gate.release();
        store.close().await.expect("observer settles before close");

        assert_eq!(store.load("cancelled-commit").unwrap().outcome, "running");
        assert_eq!(backend.committed_reports().len(), 1);
        assert!(
            store
                .state
                .lock()
                .expect(LAUNCH_REPORT_STORE_LOCK_INVARIANT)
                .writers
                .is_empty()
        );
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn cancelled_over_capacity_commit_still_finishes_retention_cleanup() {
        let (root, backend, store) = persistence_fixture("cancelled-retention");
        let store = Arc::new(store);
        let start = chrono::DateTime::parse_from_rfc3339("2026-01-01T00:00:00.000Z")
            .expect("parse start timestamp")
            .with_timezone(&chrono::Utc);
        for index in 0..MAX_STARTUP_REPORTS {
            let timestamp = (start + chrono::Duration::milliseconds(index as i64))
                .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
            store.insert_unchecked_for_test(comparison_report(
                &format!("retained-{index}"),
                &timestamp,
                90,
            ));
        }
        let gate = backend.gate_next();
        let task_store = store.clone();
        let task = tokio::spawn(async move {
            task_store
                .persist(
                    test_record("cancelled-retention"),
                    None,
                    "running".to_string(),
                    None,
                )
                .await
        });

        backend.wait_for_attempt(1).await;
        task.abort();
        assert!(task.await.expect_err("caller cancelled").is_cancelled());
        gate.release();
        store
            .close()
            .await
            .expect("close waits for detached retention");

        assert!(store.load("cancelled-retention").is_some());
        assert!(store.load("retained-0").is_none());
        assert_eq!(
            store.list_recent(MAX_STARTUP_REPORTS + 1).len(),
            MAX_STARTUP_REPORTS
        );
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn failed_retention_delete_blocks_then_retries_exact_oldest_report_on_close() {
        let (root, _backend, store) = persistence_fixture("retention-delete-retry");
        let paths = test_paths(&root);
        let start = chrono::DateTime::parse_from_rfc3339("2026-01-01T00:00:00.000Z")
            .expect("parse start timestamp")
            .with_timezone(&chrono::Utc);
        for index in 0..MAX_STARTUP_REPORTS {
            let timestamp = (start + chrono::Duration::milliseconds(index as i64))
                .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
            store.insert_unchecked_for_test(comparison_report(
                &format!("retained-{index}"),
                &timestamp,
                90,
            ));
        }
        let blocked_path = report_path(&paths, "retained-0");
        fs::create_dir_all(&blocked_path).expect("block retention deletion with directory");

        assert!(
            store
                .persist(
                    test_record("retention-delete-retry"),
                    None,
                    "running".to_string(),
                    None,
                )
                .await
                .is_err()
        );
        assert!(store.load("retention-delete-retry").is_some());
        assert!(store.load("retained-0").is_some());
        assert_eq!(
            store
                .state
                .lock()
                .expect(LAUNCH_REPORT_STORE_LOCK_INVARIANT)
                .cleanup_candidate
                .as_deref(),
            Some("retained-0")
        );

        fs::remove_dir(&blocked_path).expect("unblock retention deletion");
        store.close().await.expect("close retries exact cleanup");

        assert!(store.load("retained-0").is_none());
        assert_eq!(
            store.list_recent(MAX_STARTUP_REPORTS + 1).len(),
            MAX_STARTUP_REPORTS
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn ordered_index_selects_oldest_unprotected_report_at_absolute_capacity() {
        let store = LaunchReportStore::in_memory();
        let start = chrono::DateTime::parse_from_rfc3339("2026-01-01T00:00:00.000Z")
            .expect("parse start timestamp")
            .with_timezone(&chrono::Utc);
        for index in 0..=MAX_STARTUP_REPORTS {
            let timestamp = (start + chrono::Duration::milliseconds(index as i64))
                .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
            store.insert_unchecked_for_test(comparison_report(
                &format!("report-{index}"),
                &timestamp,
                90,
            ));
        }
        let protected = HashSet::from(["report-0".to_string()]);

        let selected = select_report_cleanup_candidate(&store.state, &protected)
            .expect("over-capacity index selects cleanup");

        assert_eq!(selected, "report-1");
        assert_eq!(store.list_recent(1)[0].session_id, "report-1024");
        assert!(store.load("report-0").is_some());
    }

    #[tokio::test]
    async fn committed_suite_claim_protects_proof_before_report_retention_runs() {
        let suites = crate::state::benchmark_suites::BenchmarkSuiteStore::new();
        let mode = "development";
        let suite_id = crate::state::benchmark_suites::derive_suite_id("instance", mode);
        let plan = crate::application::performance::benchmark_suite_plan(mode)
            .expect("development suite plan");
        let runs =
            crate::application::performance::benchmark_suite_manifest_run_inputs(mode, &plan);
        let selection = suites
            .select_reservation(&suite_id, "instance", mode, &runs, Some(0))
            .await
            .expect("select suite reservation");
        suites
            .reserve(selection, "report-0", "2026-01-01T00:00:00.000Z", false)
            .await
            .expect("commit suite claim before proof");
        let store = LaunchReportStore::in_memory();
        store.bind_proof_retention(suites.proof_retention_handle());
        let start = chrono::DateTime::parse_from_rfc3339("2026-01-01T00:00:00.000Z")
            .expect("parse start timestamp")
            .with_timezone(&chrono::Utc);
        for index in 0..=MAX_STARTUP_REPORTS {
            let timestamp = (start + chrono::Duration::milliseconds(index as i64))
                .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
            store.insert_unchecked_for_test(comparison_report(
                &format!("report-{index}"),
                &timestamp,
                90,
            ));
        }

        let mutation = store.mutation_gate.clone().lock_owned().await;
        let mutation = store
            .reconcile_cleanup_holding_gate(mutation)
            .await
            .expect("retention cleanup succeeds");
        drop(mutation);

        assert!(store.load("report-0").is_some());
        assert!(store.load("report-1").is_none());
        assert_eq!(
            store.list_recent(MAX_STARTUP_REPORTS + 1).len(),
            MAX_STARTUP_REPORTS
        );
    }

    #[tokio::test]
    async fn later_report_retries_exact_failed_revision_before_terminal_candidate() {
        let (root, backend, store) = persistence_fixture("exact-retry");
        backend.fail_next();
        assert!(
            store
                .persist(
                    test_record("exact-retry"),
                    None,
                    "running".to_string(),
                    None,
                )
                .await
                .is_err()
        );
        assert!(store.load("exact-retry").is_none());

        store
            .persist(test_record("exact-retry"), None, "failed".to_string(), None)
            .await
            .expect("retry running then commit failed");

        assert_eq!(
            backend
                .committed_reports()
                .iter()
                .map(|report| report.outcome.as_str())
                .collect::<Vec<_>>(),
            vec!["running", "failed"]
        );
        assert_eq!(store.load("exact-retry").unwrap().outcome, "failed");
        store.close().await.expect("close report store");
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn terminal_report_cannot_be_downgraded_by_late_running_revision() {
        let (root, backend, store) = persistence_fixture("terminal-downgrade");
        let terminal = store
            .persist(
                test_record("terminal-downgrade"),
                None,
                "failed".to_string(),
                None,
            )
            .await
            .expect("commit terminal report");

        let observed = store
            .persist(
                test_record("terminal-downgrade"),
                None,
                "running".to_string(),
                None,
            )
            .await
            .expect("late running is a no-op");

        assert_eq!(observed, terminal);
        assert_eq!(backend.committed_reports().len(), 1);
        assert_eq!(store.load("terminal-downgrade"), Some(terminal));
        store.close().await.expect("close report store");
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn close_retries_exact_failed_report_and_is_idempotent() {
        let (root, backend, store) = persistence_fixture("close-retry");
        backend.fail_next();
        assert!(
            store
                .persist(test_record("close-retry"), None, "failed".to_string(), None,)
                .await
                .is_err()
        );

        store.close().await.expect("close retries exact report");
        store.close().await.expect("close is idempotent");

        assert_eq!(backend.committed_reports().len(), 1);
        assert_eq!(store.load("close-retry").unwrap().outcome, "failed");
        assert!(
            store
                .persist(test_record("post-close"), None, "running".to_string(), None,)
                .await
                .is_err()
        );
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn startup_rejects_hostile_and_forged_reports_without_rewrite() {
        let root = test_root("hostile-startup");
        let paths = test_paths(&root);
        let directory = report_dir(&paths);
        fs::create_dir_all(&directory).expect("create report directory");
        let baseline = comparison_report("baseline", "2026-01-01T00:00:00.000Z", 100);
        seed_report(&directory, &baseline);
        let mut forged = comparison_report("forged", "2026-01-02T00:00:00.000Z", 120);
        forged.comparison =
            build_comparison_from_candidates(&forged, std::slice::from_ref(&baseline));
        forged
            .comparison
            .as_mut()
            .expect("comparison")
            .delta_percent += 1.0;
        seed_report(&directory, &forged);
        let mut forged_outcome =
            comparison_report("forged-outcome", "2026-01-03T00:00:00.000Z", 130);
        forged_outcome.session_outcome = Some(LaunchSessionOutcome::from_reason(
            LaunchSessionExitReason::CrashedAfterBoot,
        ));
        forged_outcome
            .session_outcome
            .as_mut()
            .expect("session outcome")
            .summary = "/home/Secret --access-token raw-secret".to_string();
        seed_report(&directory, &forged_outcome);
        let mut forged_dimensions =
            comparison_report("forged-dimensions", "2026-01-04T00:00:00.000Z", 140);
        forged_dimensions.comparison =
            build_comparison_from_candidates(&forged_dimensions, std::slice::from_ref(&baseline));
        forged_dimensions
            .comparison
            .as_mut()
            .expect("comparison")
            .baseline
            .version_id = "1.20.1".to_string();
        seed_report(&directory, &forged_dimensions);
        let mut future_baseline =
            comparison_report("future-baseline", "2026-01-05T00:00:00.000Z", 150);
        future_baseline.comparison =
            build_comparison_from_candidates(&future_baseline, std::slice::from_ref(&baseline));
        future_baseline
            .comparison
            .as_mut()
            .expect("comparison")
            .baseline_recorded_at = "2027-01-01T00:00:00.000Z".to_string();
        seed_report(&directory, &future_baseline);
        let mut contradictory = comparison_report("contradictory", "2026-01-06T00:00:00.000Z", 160);
        contradictory.outcome = "running".to_string();
        contradictory.session_outcome = Some(LaunchSessionOutcome::from_reason(
            LaunchSessionExitReason::CrashedAfterBoot,
        ));
        seed_report(&directory, &contradictory);
        let hostile_path = directory.join("hostile.json");
        fs::write(
            &hostile_path,
            br#"{"schema":"foreign","secret":"preserve"}"#,
        )
        .expect("seed hostile report");
        let hostile_bytes = fs::read(&hostile_path).expect("read hostile report");

        let store = LaunchReportStore::load_from_paths_for_test(&paths);

        assert_eq!(store.list_recent(10), vec![baseline]);
        assert_eq!(store.load_issue_count(), 6);
        assert!(
            store
                .persist(
                    test_record("new-after-rejection"),
                    None,
                    "running".to_string(),
                    None,
                )
                .await
                .is_err()
        );
        assert_eq!(
            fs::read(hostile_path).expect("reread hostile"),
            hostile_bytes
        );
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn admitted_identity_tracks_filesystem_objects_instead_of_contents() {
        let root = test_root("report-file-identity");
        fs::create_dir_all(&root).expect("create identity test directory");
        let source = root.join("source.json");
        let alias = root.join("alias.json");
        let distinct = root.join("distinct.json");
        fs::write(&source, b"same bytes").expect("write source");
        fs::hard_link(&source, &alias).expect("create source hardlink");
        fs::write(&distinct, b"same bytes").expect("write distinct file");

        let (_, source_identity) = admitted_path_snapshot(&source).unwrap();

        assert_eq!(source_identity, admitted_path_snapshot(&alias).unwrap().1);
        assert_ne!(
            source_identity,
            admitted_path_snapshot(&distinct).unwrap().1
        );

        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn startup_rejects_symlinked_report_without_following_or_rewriting_it() {
        use std::os::unix::fs::symlink;

        let root = test_root("symlink-startup");
        let paths = test_paths(&root);
        let directory = report_dir(&paths);
        fs::create_dir_all(&directory).expect("create report directory");
        let outside = root.join("outside.json");
        fs::write(&outside, b"preserve-outside").expect("seed outside file");
        symlink(&outside, directory.join("linked.json")).expect("create report symlink");

        let store = LaunchReportStore::load_from_paths_for_test(&paths);

        assert!(store.list_recent(10).is_empty());
        assert_eq!(store.load_issue_count(), 1);
        assert_eq!(
            fs::read(outside).expect("reread outside"),
            b"preserve-outside"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn startup_rejects_symlinked_report_directory_and_latches_mutation() {
        use std::os::unix::fs::symlink;

        let root = test_root("symlinked-directory-startup");
        let paths = test_paths(&root);
        let directory = report_dir(&paths);
        fs::create_dir_all(directory.parent().expect("report directory parent"))
            .expect("create report directory parent");
        let outside = root.join("outside");
        fs::create_dir_all(&outside).expect("create outside directory");
        let sentinel = outside.join("sentinel");
        fs::write(&sentinel, b"preserve-outside").expect("seed outside file");
        symlink(&outside, &directory).expect("create report directory symlink");

        let store = LaunchReportStore::load_from_paths_for_test(&paths);

        assert!(store.list_recent(10).is_empty());
        assert_eq!(store.load_issue_count(), 1);
        assert!(
            store
                .persist(
                    test_record("new-after-symlink"),
                    None,
                    "running".to_string(),
                    None,
                )
                .await
                .is_err()
        );
        assert_eq!(
            fs::read(sentinel).expect("reread outside"),
            b"preserve-outside"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn startup_load_issue_count_is_bounded() {
        let root = test_root("bounded-issues");
        let paths = test_paths(&root);
        let directory = report_dir(&paths);
        fs::create_dir_all(&directory).expect("create report directory");
        for index in 0..(MAX_LOAD_ISSUES + 4) {
            fs::write(directory.join(format!("invalid-{index}.json")), b"not-json")
                .expect("seed invalid report");
        }

        let store = LaunchReportStore::load_from_paths_for_test(&paths);

        assert_eq!(store.load_issue_count(), MAX_LOAD_ISSUES);
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn startup_file_count_overflow_latches_without_rewriting_input() {
        let root = test_root("startup-file-count-overflow");
        let paths = test_paths(&root);
        let directory = report_dir(&paths);
        fs::create_dir_all(&directory).expect("create report directory");
        for index in 0..=MAX_STARTUP_REPORTS {
            fs::write(
                directory.join(format!("overflow-{index}.json")),
                b"preserve",
            )
            .expect("seed overflow input");
        }
        let sentinel = directory.join(format!("overflow-{MAX_STARTUP_REPORTS}.json"));

        let store = LaunchReportStore::load_from_paths_for_test(&paths);

        assert!(store.load_issue_count() > 0);
        assert!(store.list_recent(1).is_empty());
        assert!(
            store
                .persist(
                    test_record("blocked-after-overflow"),
                    None,
                    "running".to_string(),
                    None,
                )
                .await
                .is_err()
        );
        assert_eq!(
            fs::read(sentinel).expect("reread overflow input"),
            b"preserve"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn launch_report_persists_json_and_lists_recent_records() {
        let root = test_root("persist-list");
        let paths = test_paths(&root);
        let mut first = test_record("first");
        first.pid = Some(11);
        first.failure = Some(LaunchFailure {
            class: LaunchFailureClass::StartupStalled,
            detail: Some("no startup activity observed".to_string()),
        });

        let first_proof =
            persist_test_report(&paths, &first, Some("2026-01-02T03:04:05.000Z"), "failed")
                .expect("persist first report");
        let second = persist_test_report(&paths, &test_record("second"), None, "running")
            .expect("persist second report");

        assert!(
            !report_path(&paths, "first")
                .with_extension("json.tmp")
                .exists()
        );
        assert!(
            !report_path(&paths, "second")
                .with_extension("json.tmp")
                .exists()
        );
        assert_eq!(first_proof.schema, LAUNCH_PROOF_SCHEMA);
        assert_eq!(first_proof.schema_version, LAUNCH_PROOF_SCHEMA_VERSION);
        assert_eq!(
            first_proof.failure_class.as_deref(),
            Some("startup_stalled")
        );
        assert_eq!(first_proof.pid, Some(11));
        assert_eq!(first_proof.boot_duration_ms, None);
        assert_eq!(first_proof.launched_at, "2026-01-02T03:04:05.000Z");
        assert_eq!(first_proof.guardian, None);
        assert_eq!(first_proof.scenario.scenario_id, "unknown_launch");
        assert_eq!(first_proof.scenario.performance_mode, "unknown");
        assert_eq!(first_proof.scenario.version_id.as_deref(), Some("1.21.1"));
        assert_eq!(first_proof.scenario.benchmark_profile, None);
        assert_eq!(first_proof.scenario.benchmark_run_type, None);
        assert_eq!(first_proof.scenario.benchmark_mode, None);
        assert_eq!(first_proof.scenario.benchmark_id, None);
        assert_eq!(first_proof.comparison, None);
        assert!(matches!(
            first_proof.device.tier.as_str(),
            "low" | "mid" | "high" | "unknown"
        ));
        assert!(
            first_proof
                .stages
                .iter()
                .any(|stage| stage.stage == "queued")
        );
        let persisted_json =
            fs::read_to_string(report_path(&paths, "first")).expect("read persisted report");
        assert!(!persisted_json.contains("command"));
        assert!(!persisted_json.contains("boot_duration_ms"));
        assert!(!persisted_json.contains("-Xmx2048M"));
        assert!(!persisted_json.contains("java_path"));

        let loaded = load_test_report(&paths, "first")
            .expect("load report")
            .expect("report exists");
        assert_eq!(loaded.session_id, "first");
        assert_eq!(loaded.outcome, "failed");

        let recent = list_test_reports(&paths, 10).expect("list reports");
        assert_eq!(recent.len(), 2);
        assert!(
            recent
                .iter()
                .any(|report| report.session_id == second.session_id)
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn launch_report_exports_redacted_stage_evidence() {
        let root = test_root("stage-evidence");
        let paths = test_paths(&root);
        let mut record = test_record("stage-evidence");
        record.stages[0].evidence = vec![
            LaunchStageEvidence {
                id: "execution_launch_command_prepared".to_string(),
                system: "execution".to_string(),
                summary: "Execution prepared a runnable launch command.".to_string(),
                details: vec![
                    "arg_count:3".to_string(),
                    r"program:C:\Users\Alice\.jdks\java.exe".to_string(),
                    "-Xmx8192M".to_string(),
                ],
            },
            LaunchStageEvidence {
                id: r"bad\path".to_string(),
                system: "execution".to_string(),
                summary: "/home/alice/.minecraft leaked".to_string(),
                details: vec!["token=secret".to_string()],
            },
        ];

        let proof = persist_test_report(&paths, &record, None, "running")
            .expect("persist stage evidence report");
        assert_eq!(proof.stages[0].evidence.len(), 1);
        assert_eq!(
            proof.stages[0].evidence[0].id,
            "execution_launch_command_prepared"
        );
        assert_eq!(proof.stages[0].evidence[0].details, vec!["arg_count:3"]);

        let persisted_json = fs::read_to_string(report_path(&paths, "stage-evidence"))
            .expect("read persisted report");
        assert!(persisted_json.contains("execution_launch_command_prepared"));
        assert!(!persisted_json.contains("Alice"));
        assert!(!persisted_json.contains("-Xmx"));
        assert!(!persisted_json.contains("token"));
        assert!(!persisted_json.contains("/home/"));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn launch_report_without_current_structural_fields_is_invalid() {
        let root = test_root("missing-current-fields");
        let paths = test_paths(&root);
        fs::create_dir_all(report_dir(&paths)).expect("create report dir");
        fs::write(
            report_path(&paths, "missing-current-fields"),
            serde_json::to_string_pretty(&json!({
                "schema": LAUNCH_PROOF_SCHEMA,
                "schema_version": LAUNCH_PROOF_SCHEMA_VERSION,
                "session_id": "missing-current-fields",
                "instance_id": "instance",
                "version_id": "1.21.1",
                "launched_at": "2026-01-01T00:00:00.000Z",
                "recorded_at": "2026-01-01T00:01:00.000Z",
                "outcome": "exited"
            }))
            .expect("serialize report"),
        )
        .expect("write report");

        let error = load_test_report(&paths, "missing-current-fields")
            .expect_err("missing scenario/device/stages should be invalid");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn launch_report_persists_boot_duration_when_session_record_has_marker_timing() {
        let root = test_root("boot-duration");
        let paths = test_paths(&root);
        let mut record = test_record("boot-duration");
        record.process_started_at_ms = Some(1_000);
        record.boot_completed_at_ms = Some(5_250);
        record.boot_duration_ms = Some(4_250);

        let proof = persist_test_report(&paths, &record, None, "running")
            .expect("persist report with boot duration");

        assert_eq!(proof.boot_duration_ms, Some(4_250));
        let persisted_json =
            fs::read_to_string(report_path(&paths, "boot-duration")).expect("read report");
        assert!(persisted_json.contains("\"boot_duration_ms\": 4250"));
        assert!(!persisted_json.contains("process_started_at_ms"));
        assert!(!persisted_json.contains("boot_completed_at_ms"));

        let loaded = load_test_report(&paths, "boot-duration")
            .expect("load report")
            .expect("report exists");
        assert_eq!(loaded.boot_duration_ms, Some(4_250));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn launch_report_persists_priority_evidence_without_empty_error_fields() {
        let root = test_root("priority-evidence");
        let paths = test_paths(&root);
        let mut record = test_record("priority-evidence");
        record.priority = Some(LaunchPriorityEvidence {
            start_mode: "below_normal_until_boot".to_string(),
            start_error: None,
            promotion: Some("promoted".to_string()),
            promotion_error: None,
        });

        let proof = persist_test_report(&paths, &record, None, "running")
            .expect("persist priority evidence");

        assert_eq!(
            proof.priority,
            Some(LaunchProofPriority {
                start_mode: "below_normal_until_boot".to_string(),
                start_error: None,
                promotion: Some("promoted".to_string()),
                promotion_error: None,
            })
        );
        let persisted_json =
            fs::read_to_string(report_path(&paths, "priority-evidence")).expect("read report");
        assert!(persisted_json.contains("\"priority\""));
        assert!(persisted_json.contains("\"start_mode\": \"below_normal_until_boot\""));
        assert!(persisted_json.contains("\"promotion\": \"promoted\""));
        assert!(!persisted_json.contains("start_error"));
        assert!(!persisted_json.contains("promotion_error"));
        assert!(!persisted_json.contains("process_started_at_ms"));
        assert!(!persisted_json.contains("boot_completed_at_ms"));

        let loaded = load_test_report(&paths, "priority-evidence")
            .expect("load report")
            .expect("report exists");
        assert_eq!(loaded.priority, proof.priority);

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn launch_report_without_optional_benchmark_metadata_loads() {
        let root = test_root("no-benchmark");
        let paths = test_paths(&root);
        fs::create_dir_all(report_dir(&paths)).expect("create report dir");
        fs::write(
            report_path(&paths, "benchmark-free"),
            serde_json::to_string_pretty(&json!({
                "schema": LAUNCH_PROOF_SCHEMA,
                "schema_version": LAUNCH_PROOF_SCHEMA_VERSION,
                "session_id": "benchmark-free",
                "instance_id": "instance",
                "version_id": "1.21.1",
                "launched_at": "2026-01-01T00:00:00.000Z",
                "recorded_at": "2026-01-01T00:01:00.000Z",
                "outcome": "running",
                "scenario": {
                    "scenario_id": "managed_launch",
                    "performance_mode": "managed",
                    "requested_memory_mb": 4096,
                    "version_id": "1.21.1"
                },
                "device": {
                    "tier": "mid",
                    "total_memory_mb": 16384,
                    "cpu_threads": 6
                },
                "stages": []
            }))
            .expect("serialize report"),
        )
        .expect("write report");

        let loaded = load_test_report(&paths, "benchmark-free")
            .expect("load report")
            .expect("report exists");

        assert_eq!(loaded.scenario.scenario_id, "managed_launch");
        assert_eq!(loaded.scenario.benchmark_profile, None);
        assert_eq!(loaded.scenario.benchmark_run_type, None);
        assert_eq!(loaded.scenario.benchmark_mode, None);
        assert_eq!(loaded.scenario.benchmark_id, None);

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn launch_report_priority_with_unknown_fields_is_invalid() {
        let root = test_root("priority-unknown-field");
        let paths = test_paths(&root);
        fs::create_dir_all(report_dir(&paths)).expect("create report dir");
        fs::write(
            report_path(&paths, "priority-unknown-field"),
            serde_json::to_string_pretty(&json!({
                "schema": LAUNCH_PROOF_SCHEMA,
                "schema_version": LAUNCH_PROOF_SCHEMA_VERSION,
                "session_id": "priority-unknown-field",
                "instance_id": "instance",
                "version_id": "1.21.1",
                "launched_at": "2026-01-01T00:00:00.000Z",
                "recorded_at": "2026-01-01T00:01:00.000Z",
                "outcome": "running",
                "scenario": {
                    "scenario_id": "managed_launch",
                    "performance_mode": "managed",
                    "requested_memory_mb": 4096,
                    "version_id": "1.21.1"
                },
                "device": {
                    "tier": "mid",
                    "total_memory_mb": 16384,
                    "cpu_threads": 6
                },
                "priority": {
                    "start_mode": "noop",
                    "unexpected": true
                },
                "stages": []
            }))
            .expect("serialize report"),
        )
        .expect("write report");

        let error = load_test_report(&paths, "priority-unknown-field")
            .expect_err("unknown priority field should be invalid");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn launch_report_nested_current_types_reject_unknown_fields() {
        let mut report = comparison_report("nested-fields", "2026-01-01T00:00:00.000Z", 90);
        report.session_outcome = Some(LaunchSessionOutcome::from_reason(
            LaunchSessionExitReason::CrashedAfterBoot,
        ));
        report.stages[0].evidence.push(LaunchStageEvidence {
            id: "process_exit".to_string(),
            system: "execution".to_string(),
            summary: "The process exited.".to_string(),
            details: Vec::new(),
        });
        let value = serde_json::to_value(report).expect("serialize report");

        for path in ["session_outcome", "stage", "evidence"] {
            let mut candidate = value.clone();
            match path {
                "session_outcome" => {
                    candidate["session_outcome"]["legacy"] = json!(true);
                }
                "stage" => candidate["stages"][0]["legacy"] = json!(true),
                "evidence" => candidate["stages"][0]["evidence"][0]["legacy"] = json!(true),
                _ => unreachable!(),
            }
            assert!(serde_json::from_value::<LaunchProofRecord>(candidate).is_err());
        }
    }

    #[test]
    fn matching_previous_launch_report_produces_comparison() {
        let current = comparison_report("current", "2026-01-02T00:00:00.000Z", 90);
        let previous = comparison_report("baseline", "2026-01-01T00:00:00.000Z", 120);

        let comparison = build_comparison_from_candidates(&current, &[previous])
            .expect("matching report comparison");

        assert_eq!(comparison.baseline_session_id, "baseline");
        assert_eq!(comparison.baseline_recorded_at, "2026-01-01T00:00:00.000Z");
        assert_eq!(comparison.matched_sample_count, 1);
        assert_eq!(comparison.metric_name, LAUNCH_STAGE_COMPARISON_METRIC_NAME);
        assert_eq!(comparison.current_value_ms, 90);
        assert_eq!(comparison.baseline_value_ms, 120);
        assert_eq!(comparison.delta_ms, -30);
        assert_eq!(comparison.delta_percent, -25.0);
    }

    #[test]
    fn qualification_baseline_check_rejects_mutated_baseline_revision() {
        let current = comparison_report("current", "2026-01-02T00:00:00.000Z", 90);
        let mut baseline = comparison_report("baseline", "2026-01-01T00:00:00.000Z", 120);
        let comparison = build_comparison_from_candidates(&current, &[baseline.clone()])
            .expect("matching report comparison");
        assert!(comparison_baseline_matches_report(&comparison, &baseline));

        baseline.stages[0].duration_ms = Some(121);
        baseline.stages[0].ended_at_ms = Some(1_121);

        assert!(!comparison_baseline_matches_report(&comparison, &baseline));
    }

    #[test]
    fn self_contained_comparison_survives_missing_baseline_and_restart() {
        let root = test_root("comparison-with-pruned-baseline");
        let paths = test_paths(&root);
        let directory = report_dir(&paths);
        fs::create_dir_all(&directory).expect("create report directory");
        let baseline = comparison_report("baseline", "2026-01-01T00:00:00.000Z", 120);
        let mut current = comparison_report("current", "2026-01-02T00:00:00.000Z", 90);
        current.comparison =
            build_comparison_from_candidates(&current, std::slice::from_ref(&baseline));
        seed_report(&directory, &current);

        let store = LaunchReportStore::load_from_paths_for_test(&paths);

        assert_eq!(store.load_issue_count(), 0);
        assert_eq!(store.load("current"), Some(current));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn three_report_history_remains_current_after_restart() {
        let root = test_root("comparison-history-restart");
        let paths = test_paths(&root);
        let directory = report_dir(&paths);
        fs::create_dir_all(&directory).expect("create report directory");
        let first = comparison_report("first", "2026-01-01T00:00:00.000Z", 120);
        let mut second = comparison_report("second", "2026-01-02T00:00:00.000Z", 100);
        second.comparison = build_comparison_from_candidates(&second, std::slice::from_ref(&first));
        let mut third = comparison_report("third", "2026-01-03T00:00:00.000Z", 90);
        third.comparison =
            build_comparison_from_candidates(&third, &[first.clone(), second.clone()]);
        for report in [&first, &second, &third] {
            seed_report(&directory, report);
        }

        let store = LaunchReportStore::load_from_paths_for_test(&paths);

        assert_eq!(store.load_issue_count(), 0);
        assert_eq!(store.list_recent(3), vec![third, second, first]);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn normal_launch_reports_without_benchmark_metadata_still_compare() {
        let current = comparison_report("current", "2026-01-02T00:00:00.000Z", 90);
        let previous = comparison_report("baseline", "2026-01-01T00:00:00.000Z", 120);

        let comparison = build_comparison_from_candidates(&current, &[previous])
            .expect("normal launch report comparison");

        assert_eq!(comparison.baseline_session_id, "baseline");
        assert_eq!(comparison.matched_sample_count, 1);
    }

    #[test]
    fn empty_or_unknown_benchmark_metadata_is_compatible_with_normal_launch_reports() {
        let current = comparison_report("current", "2026-01-02T00:00:00.000Z", 90);
        let mut previous = comparison_report("baseline", "2026-01-01T00:00:00.000Z", 120);
        previous.scenario.benchmark_profile = Some(" ".to_string());
        previous.scenario.benchmark_run_type = Some("unknown".to_string());
        previous.scenario.benchmark_mode = Some("unknown".to_string());

        let comparison = build_comparison_from_candidates(&current, &[previous])
            .expect("normal launch report comparison");

        assert_eq!(comparison.baseline_session_id, "baseline");
        assert_eq!(comparison.matched_sample_count, 1);
    }

    #[test]
    fn benchmark_launch_reports_with_matching_metadata_compare() {
        let mut current = comparison_report("current", "2026-01-02T00:00:00.000Z", 90);
        set_benchmark_metadata(&mut current, "development-default", "repeat", "development");
        let mut previous = comparison_report("baseline", "2026-01-01T00:00:00.000Z", 120);
        set_benchmark_metadata(
            &mut previous,
            "development-default",
            "repeat",
            "development",
        );

        let comparison = build_comparison_from_candidates(&current, &[previous])
            .expect("matching benchmark report comparison");

        assert_eq!(comparison.baseline_session_id, "baseline");
        assert_eq!(comparison.matched_sample_count, 1);
    }

    #[test]
    fn managed_benchmark_launch_report_compares_to_matching_vanilla_baseline() {
        let mut current = comparison_report("current", "2026-01-02T00:00:00.000Z", 90);
        set_benchmark_metadata(&mut current, "development-default", "repeat", "development");
        let mut vanilla = comparison_report("vanilla", "2026-01-01T00:00:00.000Z", 120);
        set_launch_mode(&mut vanilla, "vanilla");
        set_benchmark_metadata(&mut vanilla, "development-default", "repeat", "development");

        let comparison = build_comparison_from_candidates(&current, &[vanilla])
            .expect("matching vanilla benchmark report comparison");

        assert_eq!(comparison.baseline_session_id, "vanilla");
        assert_eq!(comparison.matched_sample_count, 1);
        assert_eq!(comparison.baseline_value_ms, 120);
    }

    #[test]
    fn managed_benchmark_launch_report_prefers_vanilla_over_newer_managed_baseline() {
        let mut current = comparison_report("current", "2026-01-03T00:00:00.000Z", 90);
        set_benchmark_metadata(&mut current, "development-default", "repeat", "development");
        let mut vanilla = comparison_report("vanilla", "2026-01-01T00:00:00.000Z", 120);
        set_launch_mode(&mut vanilla, "vanilla");
        set_benchmark_metadata(&mut vanilla, "development-default", "repeat", "development");
        let mut managed = comparison_report("managed", "2026-01-02T00:00:00.000Z", 110);
        set_benchmark_metadata(&mut managed, "development-default", "repeat", "development");

        let comparison = build_comparison_from_candidates(&current, &[vanilla, managed])
            .expect("matching benchmark report comparison");

        assert_eq!(comparison.baseline_session_id, "vanilla");
        assert_eq!(comparison.matched_sample_count, 2);
        assert_eq!(comparison.baseline_value_ms, 120);
    }

    #[test]
    fn vanilla_launch_report_does_not_compare_to_managed_launch_report() {
        let mut current = comparison_report("current", "2026-01-02T00:00:00.000Z", 90);
        set_launch_mode(&mut current, "vanilla");
        let managed = comparison_report("managed", "2026-01-01T00:00:00.000Z", 120);

        let comparison = build_comparison_from_candidates(&current, &[managed]);

        assert_eq!(comparison, None);
    }

    #[test]
    fn custom_launch_report_does_not_compare_to_managed_or_vanilla_launch_report() {
        let mut current = comparison_report("current", "2026-01-02T00:00:00.000Z", 90);
        set_launch_mode(&mut current, "custom");
        let managed = comparison_report("managed", "2026-01-01T00:00:00.000Z", 120);
        let mut vanilla = comparison_report("vanilla", "2026-01-01T00:00:01.000Z", 130);
        set_launch_mode(&mut vanilla, "vanilla");

        let comparison = build_comparison_from_candidates(&current, &[managed, vanilla]);

        assert_eq!(comparison, None);
    }

    #[test]
    fn unknown_or_empty_launch_modes_do_not_cross_compare() {
        let current = comparison_report("current", "2026-01-02T00:00:00.000Z", 90);
        let mut unknown = comparison_report("unknown", "2026-01-01T00:00:00.000Z", 120);
        set_launch_mode(&mut unknown, "unknown");
        let mut empty = comparison_report("empty", "2026-01-01T00:00:01.000Z", 130);
        empty.scenario.scenario_id.clear();
        empty.scenario.performance_mode.clear();

        let comparison = build_comparison_from_candidates(&current, &[unknown, empty]);

        assert_eq!(comparison, None);

        let mut current_unknown =
            comparison_report("current-unknown", "2026-01-02T00:00:00.000Z", 90);
        set_launch_mode(&mut current_unknown, "unknown");
        let mut vanilla = comparison_report("vanilla", "2026-01-01T00:00:00.000Z", 120);
        set_launch_mode(&mut vanilla, "vanilla");

        let comparison = build_comparison_from_candidates(&current_unknown, &[vanilla]);

        assert_eq!(comparison, None);
    }

    #[test]
    fn benchmark_launch_reports_with_different_profile_do_not_compare() {
        let mut current = comparison_report("current", "2026-01-02T00:00:00.000Z", 90);
        set_benchmark_metadata(&mut current, "development-default", "repeat", "development");
        let mut previous = comparison_report("baseline", "2026-01-01T00:00:00.000Z", 120);
        set_benchmark_metadata(&mut previous, "release-default", "repeat", "development");

        let comparison = build_comparison_from_candidates(&current, &[previous]);

        assert_eq!(comparison, None);
    }

    #[test]
    fn benchmark_launch_reports_with_different_run_type_do_not_compare() {
        let mut current = comparison_report("current", "2026-01-02T00:00:00.000Z", 90);
        set_benchmark_metadata(&mut current, "development-default", "cold", "development");
        let mut previous = comparison_report("baseline", "2026-01-01T00:00:00.000Z", 120);
        set_benchmark_metadata(
            &mut previous,
            "development-default",
            "repeat",
            "development",
        );

        let comparison = build_comparison_from_candidates(&current, &[previous]);

        assert_eq!(comparison, None);
    }

    #[test]
    fn benchmark_launch_reports_with_different_mode_do_not_compare() {
        let mut current = comparison_report("current", "2026-01-02T00:00:00.000Z", 90);
        set_benchmark_metadata(&mut current, "development-default", "repeat", "development");
        let mut previous = comparison_report("baseline", "2026-01-01T00:00:00.000Z", 120);
        set_benchmark_metadata(
            &mut previous,
            "development-default",
            "repeat",
            "release_validation",
        );

        let comparison = build_comparison_from_candidates(&current, &[previous]);

        assert_eq!(comparison, None);
    }

    #[test]
    fn benchmark_launch_report_does_not_compare_to_normal_launch_report() {
        let mut current = comparison_report("current", "2026-01-02T00:00:00.000Z", 90);
        set_benchmark_metadata(&mut current, "development-default", "repeat", "development");
        let previous = comparison_report("baseline", "2026-01-01T00:00:00.000Z", 120);

        let comparison = build_comparison_from_candidates(&current, &[previous]);

        assert_eq!(comparison, None);
    }

    #[test]
    fn matching_boot_duration_launch_report_uses_boot_duration_comparison() {
        let mut current = comparison_report("current", "2026-01-02T00:00:00.000Z", 90);
        current.boot_duration_ms = Some(50);
        let mut previous = comparison_report("baseline", "2026-01-01T00:00:00.000Z", 120);
        previous.boot_duration_ms = Some(75);

        let comparison = build_comparison_from_candidates(&current, &[previous])
            .expect("matching boot duration comparison");

        assert_eq!(comparison.baseline_session_id, "baseline");
        assert_eq!(comparison.matched_sample_count, 1);
        assert_eq!(comparison.metric_name, LAUNCH_BOOT_COMPARISON_METRIC_NAME);
        assert_eq!(comparison.current_value_ms, 50);
        assert_eq!(comparison.baseline_value_ms, 75);
        assert_eq!(comparison.delta_ms, -25);
    }

    #[test]
    fn current_boot_duration_launch_report_does_not_compare_to_stage_only_candidate() {
        let mut current = comparison_report("current", "2026-01-02T00:00:00.000Z", 90);
        current.boot_duration_ms = Some(50);
        let previous = comparison_report("baseline", "2026-01-01T00:00:00.000Z", 120);

        let comparison = build_comparison_from_candidates(&current, &[previous]);

        assert_eq!(comparison, None);
    }

    #[test]
    fn failed_current_launch_report_does_not_compare_to_matching_successful_candidate() {
        let mut current = comparison_report("current", "2026-01-02T00:00:00.000Z", 90);
        current.outcome = "failed".to_string();
        let previous = comparison_report("baseline", "2026-01-01T00:00:00.000Z", 120);

        let comparison = build_comparison_from_candidates(&current, &[previous]);

        assert_eq!(comparison, None);
    }

    #[test]
    fn failed_candidate_is_ignored_for_launch_report_comparison() {
        let current = comparison_report("current", "2026-01-03T00:00:00.000Z", 90);
        let successful = comparison_report("successful", "2026-01-01T00:00:00.000Z", 120);
        let mut failed = comparison_report("failed", "2026-01-02T00:00:00.000Z", 10);
        failed.outcome = "failed".to_string();

        let comparison = build_comparison_from_candidates(&current, &[successful, failed])
            .expect("matching successful report comparison");

        assert_eq!(comparison.baseline_session_id, "successful");
        assert_eq!(comparison.matched_sample_count, 1);
        assert_eq!(comparison.baseline_value_ms, 120);
        assert_eq!(comparison.delta_ms, -30);
    }

    #[test]
    fn failed_candidate_does_not_produce_launch_report_comparison() {
        let current = comparison_report("current", "2026-01-03T00:00:00.000Z", 90);
        let mut failed = comparison_report("failed", "2026-01-02T00:00:00.000Z", 10);
        failed.outcome = "failed".to_string();

        let comparison = build_comparison_from_candidates(&current, &[failed]);

        assert_eq!(comparison, None);
    }

    #[test]
    fn nonmatching_version_device_or_memory_does_not_compare() {
        let current = comparison_report("current", "2026-01-02T00:00:00.000Z", 90);
        let mut wrong_version = comparison_report("wrong-version", "2026-01-01T00:00:00.000Z", 120);
        wrong_version.scenario.version_id = Some("1.20.6".to_string());
        wrong_version.version_id = "1.20.6".to_string();
        let mut wrong_device = comparison_report("wrong-device", "2026-01-01T00:00:01.000Z", 120);
        wrong_device.device.tier = "high".to_string();
        let mut wrong_memory = comparison_report("wrong-memory", "2026-01-01T00:00:02.000Z", 120);
        wrong_memory.scenario.requested_memory_mb = Some(8192);

        let comparison = build_comparison_from_candidates(
            &current,
            &[wrong_version, wrong_device, wrong_memory],
        );

        assert_eq!(comparison, None);
    }

    #[test]
    fn persisted_launch_report_compares_to_previous_matching_local_report() {
        let root = test_root("persist-comparison");
        let paths = test_paths(&root);
        let context = LaunchProofContext {
            performance_mode: "managed".to_string(),
            requested_memory_mb: Some(4096),
            version_id: Some("1.21.4".to_string()),
            benchmark: None,
            resource_budget: None,
        };
        let baseline = test_record_with_stage_duration("baseline", 120);
        let current = test_record_with_stage_duration("current", 90);

        let first =
            persist_test_report_with_context(&paths, &baseline, None, "exited", Some(&context))
                .expect("persist baseline report");
        let second =
            persist_test_report_with_context(&paths, &current, None, "exited", Some(&context))
                .expect("persist current report");

        assert_eq!(first.comparison, None);
        let comparison = second.comparison.expect("persisted comparison");
        assert_eq!(comparison.baseline_session_id, "baseline");
        assert_eq!(comparison.matched_sample_count, 1);
        assert_eq!(comparison.metric_name, LAUNCH_STAGE_COMPARISON_METRIC_NAME);
        assert_eq!(comparison.current_value_ms, 90);
        assert_eq!(comparison.baseline_value_ms, 120);
        assert_eq!(comparison.delta_ms, -30);

        let persisted_json =
            fs::read_to_string(report_path(&paths, "current")).expect("read current report");
        assert!(persisted_json.contains("\"comparison\""));
        assert!(!persisted_json.contains("-Xmx2048M"));
        assert!(!persisted_json.contains("/usr/bin/java"));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn launch_report_builds_benchmark_scenario_from_context_without_sensitive_fields() {
        let root = test_root("scenario-context");
        let paths = test_paths(&root);
        let record = test_record("scenario");
        let context = LaunchProofContext {
            performance_mode: " managed ".to_string(),
            requested_memory_mb: Some(4096),
            version_id: Some("1.21.4".to_string()),
            benchmark: Some(LaunchBenchmarkMetadata::new(
                Some(" benchmark-1 "),
                Some(" dev-default\n"),
                Some(" repeat "),
                Some("release_validation"),
            )),
            resource_budget: None,
        };

        let proof = persist_test_report_with_context(
            &paths,
            &record,
            Some("2026-01-02T03:04:05.000Z"),
            "running",
            Some(&context),
        )
        .expect("persist report");

        assert_eq!(
            proof.scenario,
            LaunchProofScenario {
                scenario_id: "managed_launch".to_string(),
                performance_mode: "managed".to_string(),
                requested_memory_mb: Some(4096),
                version_id: Some("1.21.4".to_string()),
                benchmark_profile: Some("dev-default".to_string()),
                benchmark_run_type: Some("repeat".to_string()),
                benchmark_mode: Some("release_validation".to_string()),
                benchmark_id: Some("benchmark-1".to_string()),
            }
        );

        let persisted_json =
            fs::read_to_string(report_path(&paths, "scenario")).expect("read persisted report");
        assert!(persisted_json.contains("\"scenario\""));
        assert!(persisted_json.contains("\"device\""));
        assert!(persisted_json.contains("\"benchmark_profile\": \"dev-default\""));
        assert!(persisted_json.contains("\"benchmark_run_type\": \"repeat\""));
        assert!(persisted_json.contains("\"benchmark_mode\": \"release_validation\""));
        assert!(persisted_json.contains("\"benchmark_id\": \"benchmark-1\""));
        assert!(!persisted_json.contains("command"));
        assert!(!persisted_json.contains("java_path"));
        assert!(!persisted_json.contains("/usr/bin/java"));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn launch_report_persists_resource_budget_without_sensitive_fields() {
        let root = test_root("resource-budget-context");
        let paths = test_paths(&root);
        let mut record = test_record("resource-budget");
        record.command.push("-Dauth.username=Player".to_string());
        let resource_budget = LaunchProofResourceBudget {
            host_total_memory_mb: Some(8192),
            host_available_memory_mb: Some(4096),
            host_used_memory_mb: Some(4096),
            host_cpu_threads: Some(4),
            host_cpu_load_1m_x100: Some(42),
            host_cpu_load_5m_x100: Some(35),
            host_cpu_load_15m_x100: Some(21),
            launcher_process_memory_mb: Some(128),
            active_session_count: 1,
            active_install_count: 1,
            active_memory_allocation_mb: 3072,
            requested_memory_mb: Some(4096),
            estimated_remaining_memory_mb: Some(1024),
            memory_headroom_mb: 2048,
            memory_pressure: true,
            cpu_pressure: true,
            install_pressure: true,
            launch_disk_available_mb: Some(1536),
            launch_disk_headroom_mb: axial_launcher::LAUNCH_DISK_HEADROOM_MB,
            disk_pressure: true,
        };
        let context = LaunchProofContext {
            performance_mode: "managed".to_string(),
            requested_memory_mb: Some(4096),
            version_id: Some("1.21.4".to_string()),
            benchmark: None,
            resource_budget: Some(resource_budget.clone()),
        };

        let proof =
            persist_test_report_with_context(&paths, &record, None, "running", Some(&context))
                .expect("persist report");

        assert_eq!(proof.resource_budget, Some(resource_budget));
        let persisted_json = fs::read_to_string(report_path(&paths, "resource-budget"))
            .expect("read persisted report");
        assert!(persisted_json.contains("\"resource_budget\""));
        assert!(persisted_json.contains("\"host_available_memory_mb\": 4096"));
        assert!(persisted_json.contains("\"host_used_memory_mb\": 4096"));
        assert!(persisted_json.contains("\"host_cpu_load_1m_x100\": 42"));
        assert!(persisted_json.contains("\"host_cpu_load_5m_x100\": 35"));
        assert!(persisted_json.contains("\"host_cpu_load_15m_x100\": 21"));
        assert!(persisted_json.contains("\"launcher_process_memory_mb\": 128"));
        assert!(persisted_json.contains("\"active_session_count\": 1"));
        assert!(persisted_json.contains("\"estimated_remaining_memory_mb\": 1024"));
        assert!(persisted_json.contains("\"launch_disk_available_mb\": 1536"));
        assert!(persisted_json.contains("\"launch_disk_headroom_mb\": 2048"));
        assert!(persisted_json.contains("\"disk_pressure\": true"));
        assert!(!persisted_json.contains("command"));
        assert!(!persisted_json.contains("-Xmx2048M"));
        assert!(!persisted_json.contains("-Dauth.username"));
        assert!(!persisted_json.contains("Player"));
        assert!(!persisted_json.contains("java_path"));
        assert!(!persisted_json.contains("/usr/bin/java"));
        assert!(!persisted_json.contains("/tmp/natives"));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn launch_report_persists_healing_without_java_path_fragments() {
        let root = test_root("healing-privacy");
        let paths = test_paths(&root);
        let mut record = test_record("healing-privacy");
        let healing = build_healing_summary(HealingSummaryInput {
            auth_mode: "offline",
            requested_java_path: " /home/alice/.sdkman/candidates/java/21/bin/java ",
            requested_preset: "",
            effective_java_path: Some(r"C:\Users\alice\AppData\Local\VendorRuntime\java.exe"),
            effective_preset: None,
            fallback_applied: None,
            retry_count: 0,
            failure_class: None,
        })
        .expect("build healing");
        record.healing = serde_json::to_value(healing).ok();

        let proof = persist_test_report(&paths, &record, None, "running").expect("persist report");

        assert!(proof.healing.is_some());
        let persisted_json = fs::read_to_string(report_path(&paths, "healing-privacy"))
            .expect("read persisted report");
        let persisted_lower = persisted_json.to_ascii_lowercase();
        for fragment in [
            "/usr",
            "/home",
            "\\",
            "java",
            "alice",
            "sdkman",
            "candidates",
            "bin",
            "users",
            "appdata",
            "vendorruntime",
            "java.exe",
        ] {
            assert!(
                !persisted_lower.contains(fragment),
                "persisted healing leaked fragment {fragment:?}: {persisted_json}"
            );
        }
        assert!(!persisted_json.contains("requested_java_path"));
        assert!(!persisted_json.contains("effective_java_path"));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn launch_report_resource_budget_without_current_pressure_fields_is_invalid() {
        let root = test_root("resource-budget-missing-current-fields");
        let paths = test_paths(&root);
        fs::create_dir_all(report_dir(&paths)).expect("create report dir");
        fs::write(
            report_path(&paths, "resource-budget-missing-current-fields"),
            serde_json::to_string_pretty(&json!({
                "schema": LAUNCH_PROOF_SCHEMA,
                "schema_version": LAUNCH_PROOF_SCHEMA_VERSION,
                "session_id": "resource-budget-missing-current-fields",
                "instance_id": "instance",
                "version_id": "1.21.1",
                "launched_at": "2026-01-01T00:00:00.000Z",
                "recorded_at": "2026-01-01T00:01:00.000Z",
                "outcome": "running",
                "scenario": {
                    "scenario_id": "managed_launch",
                    "performance_mode": "managed",
                    "requested_memory_mb": 4096,
                    "version_id": "1.21.1"
                },
                "device": {
                    "tier": "mid",
                    "total_memory_mb": 16384,
                    "cpu_threads": 6
                },
                "resource_budget": {
                    "host_total_memory_mb": 8192,
                    "host_cpu_threads": 4,
                    "active_session_count": 1,
                    "active_install_count": 0,
                    "active_memory_allocation_mb": 2048,
                    "requested_memory_mb": 4096,
                    "estimated_remaining_memory_mb": 2048,
                    "memory_headroom_mb": 2048,
                    "memory_pressure": false,
                    "cpu_pressure": false,
                    "install_pressure": false
                },
                "stages": []
            }))
            .expect("serialize report"),
        )
        .expect("write report");

        let error = load_test_report(&paths, "resource-budget-missing-current-fields")
            .expect_err("missing current resource budget pressure fields should be invalid");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn benchmark_metadata_accepts_current_modes() {
        let development = LaunchBenchmarkMetadata::new(None, None, None, Some("development"));
        let qualification = LaunchBenchmarkMetadata::new(None, None, None, Some("qualification"));
        let release = LaunchBenchmarkMetadata::new(None, None, None, Some("release_validation"));

        assert_eq!(development.mode.as_deref(), Some("development"));
        assert_eq!(qualification.mode.as_deref(), Some("qualification"));
        assert_eq!(release.mode.as_deref(), Some("release_validation"));
    }

    #[test]
    fn benchmark_metadata_drops_sensitive_client_controlled_values() {
        let metadata = LaunchBenchmarkMetadata::new(
            Some("/home/alice/token=secret"),
            Some("C:\\Users\\Alice\\profile"),
            Some("--access-token raw-secret"),
            Some("release_validation"),
        );

        assert_eq!(metadata.benchmark_id, None);
        assert_eq!(metadata.profile, None);
        assert_eq!(metadata.run_type, None);
        assert_eq!(metadata.mode.as_deref(), Some("release_validation"));

        let context = LaunchProofContext {
            performance_mode: "managed".to_string(),
            requested_memory_mb: Some(4096),
            version_id: Some("1.21.4".to_string()),
            benchmark: Some(metadata),
            resource_budget: None,
        };
        let proof = build_record(
            &test_record("sensitive-benchmark"),
            None,
            "running",
            Some(&context),
        );
        let serialized = serde_json::to_string(&proof).expect("serialize proof");
        assert_eq!(proof.scenario.benchmark_id, None);
        assert_eq!(proof.scenario.benchmark_profile, None);
        assert_eq!(proof.scenario.benchmark_run_type, None);
        assert!(!serialized.contains("alice"));
        assert!(!serialized.contains("secret"));
    }

    #[test]
    fn launch_report_size_bound_fits_maximum_sanitized_stage_shape() {
        let mut report = comparison_report("maximum-shape", "2026-01-01T00:00:00.000Z", 90);
        let detail = "a".repeat(MAX_EXPORT_DETAIL_CHARS);
        let evidence = LaunchStageEvidence {
            id: "bounded_evidence".to_string(),
            system: "execution".to_string(),
            summary: detail.clone(),
            details: vec![detail.clone(); MAX_REPORT_EVIDENCE_DETAILS],
        };
        report.stages = (0..MAX_EXPORT_STAGES)
            .map(|index| LaunchStageRecord {
                stage: format!("stage_{index}"),
                label: "Bounded stage".to_string(),
                started_at_ms: index as u64,
                ended_at_ms: Some(index as u64 + 1),
                duration_ms: Some(1),
                result: Some("completed".to_string()),
                warnings: vec![detail.clone(); MAX_EXPORT_DETAILS],
                fallback_reason: Some(detail.clone()),
                evidence: vec![evidence.clone(); MAX_REPORT_STAGE_EVIDENCE],
            })
            .collect();

        let bytes = encode_launch_report(report).expect("maximum sanitized report fits");
        assert!(bytes.len() as u64 <= MAX_REPORT_BYTES);
    }

    #[test]
    fn launch_report_loader_rejects_file_over_size_bound() {
        let root = test_root("oversized-report");
        let paths = test_paths(&root);
        let directory = report_dir(&paths);
        fs::create_dir_all(&directory).expect("create report directory");
        fs::write(
            report_path(&paths, "oversized-report"),
            vec![b'x'; MAX_REPORT_BYTES as usize + 1],
        )
        .expect("seed oversized report");

        let error = load_test_report(&paths, "oversized-report")
            .expect_err("oversized report must be rejected");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn normal_launch_proof_context_has_no_benchmark_metadata() {
        let root = test_root("normal-context");
        let paths = test_paths(&root);
        let record = test_record("normal-context");
        let intent = LaunchIntent {
            session_id: "normal-context".to_string(),
            library_dir: root.join("library"),
            instance_id: "instance".to_string(),
            version_id: "1.21.4".to_string(),
            target_version_id: "1.21.4".to_string(),
            loader: "vanilla".to_string(),
            is_modded: false,
            username: "Player".to_string(),
            auth: axial_launcher::LaunchAuthContext::offline("Player"),
            requested_java: String::new(),
            requested_preset: String::new(),
            extra_jvm_args: Vec::new(),
            max_memory_mb: 4096,
            min_memory_mb: 1024,
            resolution: None,
            launcher_name: "axial".to_string(),
            launcher_version: "test".to_string(),
            game_dir: None,
            guardian: axial_launcher::LaunchGuardianContext::default(),
            performance_mode: "managed".to_string(),
        };
        let context = LaunchProofContext::from_intent(&intent);

        assert_eq!(context.benchmark, None);
        assert_eq!(context.resource_budget, None);

        let proof =
            persist_test_report_with_context(&paths, &record, None, "running", Some(&context))
                .expect("persist report");

        assert_eq!(proof.scenario.benchmark_profile, None);
        assert_eq!(proof.scenario.benchmark_run_type, None);
        assert_eq!(proof.scenario.benchmark_mode, None);
        assert_eq!(proof.scenario.benchmark_id, None);

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn launch_report_maps_scenarios_from_performance_mode() {
        assert_eq!(
            scenario_id_for_performance_mode("managed"),
            "managed_launch"
        );
        assert_eq!(
            scenario_id_for_performance_mode("vanilla"),
            "vanilla_launch"
        );
        assert_eq!(scenario_id_for_performance_mode("custom"), "custom_launch");
        assert_eq!(
            scenario_id_for_performance_mode("unexpected"),
            "unknown_launch"
        );
    }

    #[test]
    fn device_tier_classification_is_host_independent_and_conservative() {
        assert_eq!(classify_device_tier(None, None), "unknown");
        assert_eq!(classify_device_tier(Some(4), Some(32_768)), "low");
        assert_eq!(classify_device_tier(Some(8), Some(8_192)), "low");
        assert_eq!(classify_device_tier(Some(6), Some(16_384)), "mid");
        assert_eq!(classify_device_tier(Some(8), Some(16_384)), "mid");
        assert_eq!(classify_device_tier(Some(12), Some(32_768)), "high");
    }

    fn comparison_report(
        session_id: &str,
        recorded_at: &str,
        completed_stage_duration_ms: u64,
    ) -> LaunchProofRecord {
        LaunchProofRecord {
            schema: LAUNCH_PROOF_SCHEMA.to_string(),
            schema_version: LAUNCH_PROOF_SCHEMA_VERSION,
            session_id: session_id.to_string(),
            instance_id: "instance".to_string(),
            version_id: "1.21.4".to_string(),
            launched_at: recorded_at.to_string(),
            recorded_at: recorded_at.to_string(),
            outcome: "exited".to_string(),
            session_outcome: None,
            scenario: LaunchProofScenario {
                scenario_id: "managed_launch".to_string(),
                performance_mode: "managed".to_string(),
                requested_memory_mb: Some(4096),
                version_id: Some("1.21.4".to_string()),
                benchmark_profile: None,
                benchmark_run_type: None,
                benchmark_mode: None,
                benchmark_id: None,
            },
            device: LaunchProofDevice {
                tier: "mid".to_string(),
                total_memory_mb: Some(16_384),
                cpu_threads: Some(6),
            },
            resource_budget: None,
            pid: None,
            exit_code: Some(0),
            boot_duration_ms: None,
            priority: None,
            failure_class: None,
            failure_detail: None,
            guardian: None,
            healing: None,
            stages: vec![LaunchStageRecord {
                stage: "queued".to_string(),
                label: launch_stage_label("queued").to_string(),
                started_at_ms: 1_000,
                ended_at_ms: Some(1_000 + completed_stage_duration_ms),
                duration_ms: Some(completed_stage_duration_ms),
                result: Some("ok".to_string()),
                warnings: Vec::new(),
                fallback_reason: None,
                evidence: Vec::new(),
            }],
            comparison: None,
        }
    }

    fn set_benchmark_metadata(
        report: &mut LaunchProofRecord,
        profile: &str,
        run_type: &str,
        mode: &str,
    ) {
        report.scenario.benchmark_profile = Some(profile.to_string());
        report.scenario.benchmark_run_type = Some(run_type.to_string());
        report.scenario.benchmark_mode = Some(mode.to_string());
    }

    fn set_launch_mode(report: &mut LaunchProofRecord, mode: &str) {
        report.scenario.scenario_id = scenario_id_for_performance_mode(mode).to_string();
        report.scenario.performance_mode = mode.to_string();
    }

    fn test_record(session_id: &str) -> LaunchSessionRecord {
        LaunchSessionRecord {
            session_id: SessionId(session_id.to_string()),
            instance_id: "instance".to_string(),
            version_id: "1.21.1".to_string(),
            launched_at: Some("2026-01-01T00:00:00.000Z".to_string()),
            benchmark: None,
            state: LaunchState::Queued,
            pid: None,
            process_started_at_ms: None,
            boot_completed_at_ms: None,
            boot_duration_ms: None,
            priority: None,
            exit_code: None,
            command: vec!["java".to_string(), "-Xmx2048M".to_string()],
            java_path: Some("/usr/bin/java".to_string()),
            natives_dir: Some("/tmp/natives".to_string()),
            failure: None,
            crash_evidence: None,
            healing: Some(json!({ "fallback_applied": "test fallback" })),
            guardian: Some(json!({ "mode": "managed" })),
            outcome: None,
            stages: vec![LaunchStageRecord {
                stage: "queued".to_string(),
                label: launch_stage_label("queued").to_string(),
                started_at_ms: 1,
                ended_at_ms: Some(2),
                duration_ms: Some(1),
                result: Some("ok".to_string()),
                warnings: Vec::new(),
                fallback_reason: None,
                evidence: Vec::new(),
            }],
        }
    }

    fn test_record_with_stage_duration(session_id: &str, duration_ms: u64) -> LaunchSessionRecord {
        let mut record = test_record(session_id);
        record.stages[0].started_at_ms = 1_000;
        record.stages[0].ended_at_ms = Some(1_000 + duration_ms);
        record.stages[0].duration_ms = Some(duration_ms);
        record
    }

    fn seed_report(directory: &Path, report: &LaunchProofRecord) {
        fs::write(
            directory.join(report_filename(&report.session_id)),
            encode_launch_report(report.clone()).expect("encode report fixture"),
        )
        .expect("write report fixture");
    }

    fn test_store_with_recording_backend(
        paths: &AppPaths,
    ) -> (Arc<RecordingBackend>, LaunchReportStore) {
        let backend = Arc::new(RecordingBackend::new());
        let coordinator = PersistenceCoordinator::for_test(
            backend.clone(),
            std::time::Duration::ZERO,
            std::time::Duration::ZERO,
        );
        let store = LaunchReportStore::load_from_paths_with_coordinator(paths, coordinator)
            .expect("load launch report store");
        (backend, store)
    }

    fn persistence_fixture(name: &str) -> (PathBuf, Arc<RecordingBackend>, LaunchReportStore) {
        let root = test_root(name);
        let paths = test_paths(&root);
        let (backend, store) = test_store_with_recording_backend(&paths);
        (root, backend, store)
    }

    fn test_root(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "axial-launch-reports-{name}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|value| value.as_nanos())
                .unwrap_or_default()
        ))
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
