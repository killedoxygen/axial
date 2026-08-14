use super::{
    INSTALL_FAILURE_MESSAGE, InstallJournalReconciliation, InstallProgressStepViewModel,
    InstallProgressViewModel, reconcile_install_journal_transition,
};
use crate::execution::{ExecutionFact, ExecutionFactKind};
use crate::guardian::{
    DiagnosisId, GuardianActionKind, GuardianDomain, GuardianInstallAssessment,
    GuardianInstallOutcomeMemoryPersistence, GuardianMode, GuardianPolicyContext,
    OperationEvidenceBatch, assess_install_failure, guardian_install_outcome_from_terminal,
};
use crate::observability::{
    EvidenceField, EvidenceSensitivity, RedactionAudience, evidence_text_looks_sensitive,
    sanitize_evidence_token, sanitize_public_diagnostic_text,
};
use crate::state::contracts::{
    CommandKind, ContentDownloadMetrics, DurableGuardianEvidence, JournalId, OperationId,
    OperationJournalEntry, OperationJournalStep, OperationOutcome, OperationPhase, OperationStatus,
    OperationStepMetrics, OperationStepResult, OwnershipClass, RollbackState, StabilizationSystem,
    TargetDescriptor, TargetKind,
};
use crate::state::failure_memory::{
    FailureMemoryActionOutcome, FailureMemoryKey, FailureMemoryStoreError,
    GuardianFailureMemoryEntry,
};
use crate::state::{
    GuardianFailureMemoryStore, InstallProgressRecord, OperationJournalStore,
    OperationJournalStoreError, ProducerLease, operation_journal_completed_step_is_visible,
};
use axial_minecraft::LoaderInstallFailureKind;
use axial_minecraft::download::{ExecutionDownloadFact, ExecutionDownloadFactKind};
use axial_minecraft::loaders::LoaderActiveInstallFailure;
use axial_minecraft::{
    DownloadError, DownloadFileFailureClass, DownloadProgress, LoaderBuildRecord,
    LoaderComponentId, ManagedInstallPublicationEvidenceId, RuntimeSourceFailureKind,
    installed_version_id_for, parse_build_id,
};
use serde_json::{Value, json};
use std::collections::{BTreeSet, HashSet};
use std::io;
use std::sync::Arc;
use std::time::Duration;
use tracing::warn;

const PROVIDER_FAILURE_SUPPRESSION_COOLDOWN_MINUTES: i64 = 5;
const PROVIDER_FAILURE_MEMORY_SOURCE: &str = "install_provider";
const INSTALL_GUARDIAN_MEMORY_RETRY_DELAY: Duration = Duration::from_millis(100);
const INSTALL_GUARDIAN_MEMORY_RETRY_ATTEMPTS: usize = 3;
const COALESCED_PROGRESS_EVENT_INTERVAL: usize = 25;
const ROSETTA_INSTALL_COMMAND: &str = "softwareupdate --install-rosetta --agree-to-license";
const ROSETTA_REQUIRED_INSTALL_GUIDANCE: &str = "Install Rosetta 2 by running `softwareupdate --install-rosetta --agree-to-license` in Terminal, then retry.";
const RUNTIME_UNAVAILABLE_INSTALL_FAILURE_MESSAGE_PREFIX: &str =
    "This Minecraft version needs a Java runtime that is not available for this device.";
const INSTALL_KIND_VANILLA_FACT: &str = "install_kind:vanilla";
const INSTALL_KIND_LOADER_FACT: &str = "install_kind:loader";
const INSTALL_VERSION_ID_FACT_PREFIX: &str = "install_version_id:";
const LOADER_COMPONENT_FACT_PREFIX: &str = "loader_component:";
const LOADER_BUILD_ID_FACT_PREFIX: &str = "loader_build_id:";
const INSTALL_PUBLICATION_FACT_PREFIX: &str = "install_publication:";
const INSTALL_PUBLICATION_VERSION_ID_FACT_PREFIX: &str = "install_publication_version_id:";
const INSTALL_PUBLICATION_EVIDENCE_FACT_PREFIX: &str = "install_publication_evidence:";
const INSTALL_ACTIVATION_CONTRACT_FACT_PREFIX: &str = "install_activation_contract:";
const INSTALL_PUBLICATION_COMMITTED_STEP: &str = "install_publication_committed";
const INSTALL_BASE_PUBLICATION_COMMITTED_STEP: &str = "install_base_publication_committed";
const INSTALL_CHILD_PUBLICATION_COMMITTED_STEP: &str = "install_child_publication_committed";
const INSTALL_PUBLICATION_ROLLED_BACK_STEP: &str = "install_publication_rolled_back";
const INSTALL_RECOVERING_STEP: &str = "install_progress_recovering";

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(super) enum InstallJournalIdentity {
    Vanilla {
        version_id: String,
    },
    Loader {
        target_version_id: String,
        component_id: LoaderComponentId,
        build_id: String,
        base_version_id: String,
    },
}

impl InstallJournalIdentity {
    pub(super) fn vanilla(version_id: impl Into<String>) -> Self {
        Self::Vanilla {
            version_id: version_id.into(),
        }
    }

    pub(super) fn loader(build: &LoaderBuildRecord) -> Result<Self, &'static str> {
        let (component_id, base_version_id, loader_version) =
            parse_build_id(&build.build_id).ok_or("loader build id is not canonical")?;
        if component_id != build.component_id
            || base_version_id != build.minecraft_version
            || loader_version != build.loader_version
        {
            return Err("loader build identity is inconsistent");
        }
        let target_version_id = installed_version_id_for(
            component_id,
            &build.minecraft_version,
            &build.loader_version,
        )
        .map_err(|_| "loader installed version id is invalid")?;
        if target_version_id != build.version_id {
            return Err("loader target version id is inconsistent");
        }
        Ok(Self::Loader {
            target_version_id,
            component_id,
            build_id: build.build_id.clone(),
            base_version_id,
        })
    }

    pub(super) fn target_version_id(&self) -> &str {
        match self {
            Self::Vanilla { version_id } => version_id,
            Self::Loader {
                target_version_id, ..
            } => target_version_id,
        }
    }

    fn planned_facts(&self) -> Vec<String> {
        match self {
            Self::Vanilla { version_id } => vec![
                INSTALL_KIND_VANILLA_FACT.to_string(),
                format!("{INSTALL_VERSION_ID_FACT_PREFIX}{version_id}"),
            ],
            Self::Loader {
                target_version_id,
                component_id,
                build_id,
                ..
            } => vec![
                INSTALL_KIND_LOADER_FACT.to_string(),
                format!("{INSTALL_VERSION_ID_FACT_PREFIX}{target_version_id}"),
                format!("{LOADER_COMPONENT_FACT_PREFIX}{}", component_id.as_str()),
                format!("{LOADER_BUILD_ID_FACT_PREFIX}{build_id}"),
            ],
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum InstallPublicationCheckpointKind {
    Committed,
    BaseCommitted,
    ChildCommitted,
    RolledBack,
}

impl InstallPublicationCheckpointKind {
    fn fact(self) -> &'static str {
        match self {
            Self::Committed => "committed",
            Self::BaseCommitted => "base_committed",
            Self::ChildCommitted => "child_committed",
            Self::RolledBack => "rolled_back",
        }
    }

    fn step_id(self) -> &'static str {
        match self {
            Self::Committed => INSTALL_PUBLICATION_COMMITTED_STEP,
            Self::BaseCommitted => INSTALL_BASE_PUBLICATION_COMMITTED_STEP,
            Self::ChildCommitted => INSTALL_CHILD_PUBLICATION_COMMITTED_STEP,
            Self::RolledBack => INSTALL_PUBLICATION_ROLLED_BACK_STEP,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct InstallPublicationCheckpoint {
    pub(super) kind: InstallPublicationCheckpointKind,
    pub(super) version_id: String,
    pub(super) evidence: ManagedInstallPublicationEvidenceId,
    pub(super) activation_contract_id: Option<axial_minecraft::ManagedInstallActivationContractId>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct RecoveringInstallJournal {
    pub(super) install_id: String,
    pub(super) operation_id: OperationId,
    pub(super) identity: InstallJournalIdentity,
    pub(super) checkpoints: Vec<InstallPublicationCheckpoint>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum RecoveringInstallJournalError {
    Malformed,
    DuplicateInstallId,
    DuplicateOperationId,
    DuplicateIdentity,
    DuplicateRootLane,
}

pub(super) fn recovering_install_journals(
    journals: &OperationJournalStore,
) -> Result<Vec<RecoveringInstallJournal>, RecoveringInstallJournalError> {
    let mut install_ids = BTreeSet::new();
    let mut operation_ids = BTreeSet::new();
    let mut identities = HashSet::new();
    let mut recovering = Vec::new();

    for entry in journals.matching_entries(|entry| {
        entry.command == CommandKind::InstallVersion && !install_journal_is_terminal(entry.status)
    }) {
        let journal = parse_recovering_install_journal(&entry)?;
        if !install_ids.insert(journal.install_id.clone()) {
            return Err(RecoveringInstallJournalError::DuplicateInstallId);
        }
        if !operation_ids.insert(journal.operation_id.clone()) {
            return Err(RecoveringInstallJournalError::DuplicateOperationId);
        }
        if !identities.insert(journal.identity.clone()) {
            return Err(RecoveringInstallJournalError::DuplicateIdentity);
        }
        recovering.push(journal);
    }
    if recovering.len() > 1 {
        return Err(RecoveringInstallJournalError::DuplicateRootLane);
    }
    recovering.sort_by(|left, right| left.operation_id.cmp(&right.operation_id));
    Ok(recovering)
}

pub(super) fn recovering_install_journal(
    journals: &OperationJournalStore,
    operation_id: &OperationId,
) -> Result<RecoveringInstallJournal, RecoveringInstallJournalError> {
    let entry = journals
        .get(operation_id)
        .filter(|entry| {
            entry.command == CommandKind::InstallVersion
                && !install_journal_is_terminal(entry.status)
        })
        .ok_or(RecoveringInstallJournalError::Malformed)?;
    parse_recovering_install_journal(&entry)
}

fn parse_recovering_install_journal(
    entry: &OperationJournalEntry,
) -> Result<RecoveringInstallJournal, RecoveringInstallJournalError> {
    if !matches!(
        entry.status,
        OperationStatus::Planned | OperationStatus::Running
    ) || entry.targets.len() != 2
        || entry.planned_steps.len() != 1
    {
        return Err(RecoveringInstallJournalError::Malformed);
    }
    let install_id = entry
        .targets
        .iter()
        .find(|target| {
            target.system == StabilizationSystem::Application
                && target.kind == TargetKind::Session
                && target.ownership == OwnershipClass::LauncherManaged
        })
        .map(|target| target.id.clone())
        .filter(|install_id| canonical_install_session_id(install_id))
        .ok_or(RecoveringInstallJournalError::Malformed)?;
    let target_version_id = entry
        .targets
        .iter()
        .find(|target| {
            target.system == StabilizationSystem::Application
                && target.kind == TargetKind::Version
                && target.ownership == OwnershipClass::LauncherManaged
        })
        .map(|target| target.id.clone())
        .ok_or(RecoveringInstallJournalError::Malformed)?;
    let planned = &entry.planned_steps[0];
    if planned.step_id != "install_version"
        || planned.phase != OperationPhase::Planning
        || planned.result != OperationStepResult::Planned
        || planned.changed_target.is_some()
        || planned.rollback != RollbackState::NotApplicable
    {
        return Err(RecoveringInstallJournalError::Malformed);
    }
    let identity = parse_install_journal_identity(&planned.generated_facts, &target_version_id)
        .ok_or(RecoveringInstallJournalError::Malformed)?;
    let expected = planned_install_journal_for_session(&entry.operation_id, &install_id, &identity);
    let mut expected_current = expected;
    expected_current.status = entry.status;
    expected_current.completed_steps = entry.completed_steps.clone();
    if !entry.matches_store_entry(&expected_current) {
        return Err(RecoveringInstallJournalError::Malformed);
    }
    let checkpoints = parse_install_publication_checkpoints(entry, &identity)?;
    Ok(RecoveringInstallJournal {
        install_id,
        operation_id: entry.operation_id.clone(),
        identity,
        checkpoints,
    })
}

fn parse_install_journal_identity(
    facts: &[String],
    target_version_id: &str,
) -> Option<InstallJournalIdentity> {
    match facts {
        [kind, version] if kind == INSTALL_KIND_VANILLA_FACT => {
            let version_id = version.strip_prefix(INSTALL_VERSION_ID_FACT_PREFIX)?;
            if !axial_config::instances::is_safe_version_id(version_id) {
                return None;
            }
            let identity = InstallJournalIdentity::vanilla(version_id);
            (install_version_target(identity.target_version_id()).id == target_version_id
                && identity.planned_facts() == facts)
                .then_some(identity)
        }
        [kind, version, component, build] if kind == INSTALL_KIND_LOADER_FACT => {
            let encoded_target = version.strip_prefix(INSTALL_VERSION_ID_FACT_PREFIX)?;
            let encoded_component = component.strip_prefix(LOADER_COMPONENT_FACT_PREFIX)?;
            let build_id = build.strip_prefix(LOADER_BUILD_ID_FACT_PREFIX)?;
            let component_id = LoaderComponentId::parse(encoded_component)?;
            let (parsed_component, base_version_id, loader_version) = parse_build_id(build_id)?;
            let canonical_target =
                installed_version_id_for(component_id, &base_version_id, &loader_version).ok()?;
            if component_id != parsed_component
                || encoded_target != canonical_target
                || target_version_id != install_version_target(&canonical_target).id
            {
                return None;
            }
            let identity = InstallJournalIdentity::Loader {
                target_version_id: canonical_target,
                component_id,
                build_id: build_id.to_string(),
                base_version_id,
            };
            (identity.planned_facts() == facts).then_some(identity)
        }
        _ => None,
    }
}

fn parse_install_publication_checkpoints(
    entry: &OperationJournalEntry,
    identity: &InstallJournalIdentity,
) -> Result<Vec<InstallPublicationCheckpoint>, RecoveringInstallJournalError> {
    let mut checkpoints = Vec::new();
    let mut step_ids = BTreeSet::new();
    let mut recovering_seen = false;
    for step in &entry.completed_steps {
        if !step_ids.insert(step.step_id.as_str()) {
            return Err(RecoveringInstallJournalError::Malformed);
        }
        if let Some(kind) = publication_checkpoint_kind(step.step_id.as_str()) {
            let checkpoint = parse_install_publication_checkpoint(step, kind, identity)?;
            checkpoints.push(checkpoint);
            continue;
        }
        if !canonical_nonterminal_install_progress_step(step) {
            return Err(RecoveringInstallJournalError::Malformed);
        }
        recovering_seen |= step.step_id == INSTALL_RECOVERING_STEP;
    }
    if (!entry.completed_steps.is_empty() && entry.status != OperationStatus::Running)
        || !checkpoint_sequence_matches_identity(&checkpoints, identity)
    {
        return Err(RecoveringInstallJournalError::Malformed);
    }
    if !checkpoints.is_empty() && !recovering_seen {
        return Err(RecoveringInstallJournalError::Malformed);
    }
    Ok(checkpoints)
}

fn checkpoint_sequence_matches_identity(
    checkpoints: &[InstallPublicationCheckpoint],
    identity: &InstallJournalIdentity,
) -> bool {
    match identity {
        InstallJournalIdentity::Vanilla { .. } => matches!(
            checkpoints,
            [] | [InstallPublicationCheckpoint {
                kind: InstallPublicationCheckpointKind::Committed
                    | InstallPublicationCheckpointKind::RolledBack,
                ..
            }]
        ),
        InstallJournalIdentity::Loader {
            target_version_id,
            base_version_id,
            ..
        } => match checkpoints {
            [] => true,
            [
                InstallPublicationCheckpoint {
                    kind: InstallPublicationCheckpointKind::BaseCommitted,
                    version_id,
                    ..
                },
            ] => version_id == base_version_id,
            [
                InstallPublicationCheckpoint {
                    kind: InstallPublicationCheckpointKind::RolledBack,
                    version_id,
                    ..
                },
            ] => version_id == base_version_id,
            [
                InstallPublicationCheckpoint {
                    kind: InstallPublicationCheckpointKind::BaseCommitted,
                    version_id: base_checkpoint_version_id,
                    ..
                },
                InstallPublicationCheckpoint {
                    kind: InstallPublicationCheckpointKind::ChildCommitted,
                    version_id: child_checkpoint_version_id,
                    ..
                },
            ] => {
                base_checkpoint_version_id == base_version_id
                    && child_checkpoint_version_id == target_version_id
            }
            [
                InstallPublicationCheckpoint {
                    kind: InstallPublicationCheckpointKind::BaseCommitted,
                    version_id: base_checkpoint_version_id,
                    ..
                },
                InstallPublicationCheckpoint {
                    kind: InstallPublicationCheckpointKind::RolledBack,
                    version_id: rollback_version_id,
                    ..
                },
            ] => {
                base_checkpoint_version_id == base_version_id
                    && rollback_version_id == target_version_id
            }
            _ => false,
        },
    }
}

fn canonical_nonterminal_install_progress_phase(step: &OperationJournalStep) -> Option<&str> {
    let phase = step.step_id.strip_prefix("install_progress_")?;
    (!phase.is_empty()
        && step.phase
            == install_operation_phase(&DownloadProgress {
                phase: phase.to_string(),
                current: 0,
                total: 0,
                file: None,
                error: None,
                done: false,
                bytes_done: None,
                bytes_total: None,
            })
        && step.result == OperationStepResult::Completed
        && step.changed_target.is_none()
        && step.generated_facts == [format!("install_phase:{phase}")]
        && step.rollback == RollbackState::NotApplicable)
        .then_some(phase)
}

fn canonical_nonterminal_install_progress_step(step: &OperationJournalStep) -> bool {
    canonical_nonterminal_install_progress_phase(step).is_some()
}

fn publication_checkpoint_kind(step_id: &str) -> Option<InstallPublicationCheckpointKind> {
    match step_id {
        INSTALL_PUBLICATION_COMMITTED_STEP => Some(InstallPublicationCheckpointKind::Committed),
        INSTALL_BASE_PUBLICATION_COMMITTED_STEP => {
            Some(InstallPublicationCheckpointKind::BaseCommitted)
        }
        INSTALL_CHILD_PUBLICATION_COMMITTED_STEP => {
            Some(InstallPublicationCheckpointKind::ChildCommitted)
        }
        INSTALL_PUBLICATION_ROLLED_BACK_STEP => Some(InstallPublicationCheckpointKind::RolledBack),
        _ => None,
    }
}

fn parse_install_publication_checkpoint(
    step: &OperationJournalStep,
    kind: InstallPublicationCheckpointKind,
    identity: &InstallJournalIdentity,
) -> Result<InstallPublicationCheckpoint, RecoveringInstallJournalError> {
    let expected_phase = if kind == InstallPublicationCheckpointKind::RolledBack {
        OperationPhase::RollingBack
    } else {
        OperationPhase::Installing
    };
    let expected_rollback = if kind == InstallPublicationCheckpointKind::RolledBack {
        RollbackState::Applied
    } else {
        RollbackState::NotApplicable
    };
    let (publication, version, evidence, activation_contract_id) =
        match (kind, step.generated_facts.as_slice()) {
            (
                InstallPublicationCheckpointKind::Committed
                | InstallPublicationCheckpointKind::BaseCommitted
                | InstallPublicationCheckpointKind::ChildCommitted,
                [publication, version, evidence, activation_contract],
            ) => {
                let activation_contract_id = activation_contract
                    .strip_prefix(INSTALL_ACTIVATION_CONTRACT_FACT_PREFIX)
                    .and_then(|value| {
                        axial_minecraft::ManagedInstallActivationContractId::parse(value).ok()
                    })
                    .ok_or(RecoveringInstallJournalError::Malformed)?;
                (publication, version, evidence, Some(activation_contract_id))
            }
            (InstallPublicationCheckpointKind::RolledBack, [publication, version, evidence]) => {
                (publication, version, evidence, None)
            }
            _ => return Err(RecoveringInstallJournalError::Malformed),
        };
    let version_id = version
        .strip_prefix(INSTALL_PUBLICATION_VERSION_ID_FACT_PREFIX)
        .ok_or(RecoveringInstallJournalError::Malformed)?;
    let evidence = evidence
        .strip_prefix(INSTALL_PUBLICATION_EVIDENCE_FACT_PREFIX)
        .and_then(|value| ManagedInstallPublicationEvidenceId::parse(value).ok())
        .ok_or(RecoveringInstallJournalError::Malformed)?;
    if step.phase != expected_phase
        || step.result != OperationStepResult::Completed
        || step.changed_target.as_ref() != Some(&install_version_target(version_id))
        || step.rollback != expected_rollback
        || publication != &format!("{INSTALL_PUBLICATION_FACT_PREFIX}{}", kind.fact())
        || !evidence.matches_version_id(version_id)
        || !checkpoint_version_matches_identity(kind, version_id, identity)
    {
        return Err(RecoveringInstallJournalError::Malformed);
    }
    Ok(InstallPublicationCheckpoint {
        kind,
        version_id: version_id.to_string(),
        evidence,
        activation_contract_id,
    })
}

fn checkpoint_version_matches_identity(
    kind: InstallPublicationCheckpointKind,
    version_id: &str,
    identity: &InstallJournalIdentity,
) -> bool {
    match (kind, identity) {
        (
            InstallPublicationCheckpointKind::Committed
            | InstallPublicationCheckpointKind::RolledBack,
            InstallJournalIdentity::Vanilla {
                version_id: expected,
            },
        ) => version_id == expected,
        (
            InstallPublicationCheckpointKind::BaseCommitted,
            InstallJournalIdentity::Loader {
                base_version_id, ..
            },
        ) => version_id == base_version_id,
        (
            InstallPublicationCheckpointKind::ChildCommitted,
            InstallJournalIdentity::Loader {
                target_version_id, ..
            },
        ) => version_id == target_version_id,
        (
            InstallPublicationCheckpointKind::RolledBack,
            InstallJournalIdentity::Loader {
                base_version_id,
                target_version_id,
                ..
            },
        ) => version_id == base_version_id || version_id == target_version_id,
        _ => false,
    }
}

fn canonical_install_session_id(install_id: &str) -> bool {
    ["install-", "loader-install-"].into_iter().any(|prefix| {
        install_id.strip_prefix(prefix).is_some_and(|suffix| {
            suffix.len() == 32
                && suffix
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        })
    })
}

pub(super) async fn record_install_publication_checkpoint(
    journals: &OperationJournalStore,
    operation_id: &OperationId,
    checkpoint: &InstallPublicationCheckpoint,
) -> Result<(), OperationJournalStoreError> {
    let step = install_publication_checkpoint_step(checkpoint)?;
    loop {
        match journals
            .record_idempotent_checkpoint(operation_id, step.clone())
            .await
        {
            Ok(()) => return Ok(()),
            Err(error) => {
                match reconcile_install_journal_error(journals, operation_id, error, |entry| {
                    operation_journal_completed_step_is_visible(entry, &step)
                })
                .await?
                {
                    InstallJournalReconciliation::MutationCommitted => return Ok(()),
                    InstallJournalReconciliation::RetryMutation => {}
                }
            }
        }
    }
}

fn install_publication_checkpoint_step(
    checkpoint: &InstallPublicationCheckpoint,
) -> Result<OperationJournalStep, OperationJournalStoreError> {
    let mut step = install_journal_step(
        checkpoint.kind.step_id(),
        if checkpoint.kind == InstallPublicationCheckpointKind::RolledBack {
            OperationPhase::RollingBack
        } else {
            OperationPhase::Installing
        },
        OperationStepResult::Completed,
        Some(install_version_target(&checkpoint.version_id)),
    );
    step.rollback = if checkpoint.kind == InstallPublicationCheckpointKind::RolledBack {
        RollbackState::Applied
    } else {
        RollbackState::NotApplicable
    };
    step.generated_facts = vec![
        format!(
            "{INSTALL_PUBLICATION_FACT_PREFIX}{}",
            checkpoint.kind.fact()
        ),
        format!(
            "{INSTALL_PUBLICATION_VERSION_ID_FACT_PREFIX}{}",
            checkpoint.version_id
        ),
        format!(
            "{INSTALL_PUBLICATION_EVIDENCE_FACT_PREFIX}{}",
            checkpoint.evidence.as_str()
        ),
    ];
    match (checkpoint.kind, checkpoint.activation_contract_id.as_ref()) {
        (
            InstallPublicationCheckpointKind::Committed
            | InstallPublicationCheckpointKind::BaseCommitted
            | InstallPublicationCheckpointKind::ChildCommitted,
            Some(activation_contract_id),
        ) => step.generated_facts.push(format!(
            "{INSTALL_ACTIVATION_CONTRACT_FACT_PREFIX}{activation_contract_id}"
        )),
        (InstallPublicationCheckpointKind::RolledBack, None) => {}
        _ => {
            return Err(OperationJournalStoreError::Persistence(io::Error::new(
                io::ErrorKind::InvalidInput,
                "install publication checkpoint activation contract is invalid",
            )));
        }
    }
    Ok(step)
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ProviderFailureObservationWindow {
    observed_at: String,
    suppression_until: String,
}

impl ProviderFailureObservationWindow {
    fn from_observed_at(observed_at: &str) -> Option<Self> {
        let observed_at = chrono::DateTime::parse_from_rfc3339(observed_at)
            .ok()?
            .with_timezone(&chrono::Utc);
        let suppression_until = observed_at.checked_add_signed(chrono::Duration::minutes(
            PROVIDER_FAILURE_SUPPRESSION_COOLDOWN_MINUTES,
        ))?;
        Some(Self {
            observed_at: observed_at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            suppression_until: suppression_until
                .to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        })
    }
}
const RUNTIME_ROSETTA_REQUIRED_INSTALL_FAILURE_MESSAGE_PREFIX: &str =
    "This Minecraft version needs Rosetta 2 on Apple Silicon Macs.";

#[derive(Default)]
pub(crate) struct ContentDownloadFactAccumulator {
    facts: Vec<ExecutionDownloadFact>,
    counts: [usize; 13],
}

#[derive(Debug, Default)]
pub struct InstallProgressJournalTracker {
    recorded_nonterminal_phases: BTreeSet<String>,
}

impl InstallProgressJournalTracker {
    pub(super) fn from_install_journal(
        journals: &OperationJournalStore,
        operation_id: &OperationId,
    ) -> Self {
        let recorded_nonterminal_phases = journals
            .get(operation_id)
            .into_iter()
            .flat_map(|entry| entry.completed_steps)
            .filter_map(|step| {
                canonical_nonterminal_install_progress_phase(&step).map(str::to_owned)
            })
            .collect();
        Self {
            recorded_nonterminal_phases,
        }
    }

    fn contains(&self, phase: &str) -> bool {
        self.recorded_nonterminal_phases.contains(phase)
    }

    fn record(&mut self, phase: String) {
        self.recorded_nonterminal_phases.insert(phase);
    }
}

impl ContentDownloadFactAccumulator {
    pub(crate) fn record(&mut self, fact: ExecutionDownloadFact) {
        self.counts[execution_download_fact_kind_index(fact.kind)] =
            self.counts[execution_download_fact_kind_index(fact.kind)].saturating_add(1);
        self.facts.push(fact);
    }

    pub(crate) fn facts(&self) -> Vec<ExecutionDownloadFact> {
        self.facts.clone()
    }

    pub(crate) fn metrics(&self) -> Result<ContentDownloadMetrics, OperationJournalStoreError> {
        let counts = self.counts.map(|count| {
            u64::try_from(count).map_err(|_| OperationJournalStoreError::InvalidOperationMetrics)
        });
        let [
            checksum_mismatch,
            metadata_invalid,
            metadata_missing,
            interrupted,
            network_failure,
            permission_failure,
            promote_failed,
            provider_failure,
            size_mismatch,
            temp_discarded,
            temp_write_failed,
            written_to_temp,
            promoted,
        ] = counts;
        Ok(ContentDownloadMetrics::new(
            checksum_mismatch?,
            metadata_invalid?,
            metadata_missing?,
            interrupted?,
            network_failure?,
            permission_failure?,
            promote_failed?,
            provider_failure?,
            size_mismatch?,
            temp_discarded?,
            temp_write_failed?,
            written_to_temp?,
            promoted?,
        ))
    }
}

const EXECUTION_DOWNLOAD_FACT_KINDS: [ExecutionDownloadFactKind; 13] = [
    ExecutionDownloadFactKind::ChecksumMismatch,
    ExecutionDownloadFactKind::MetadataInvalid,
    ExecutionDownloadFactKind::MetadataMissing,
    ExecutionDownloadFactKind::Interrupted,
    ExecutionDownloadFactKind::NetworkFailure,
    ExecutionDownloadFactKind::PermissionFailure,
    ExecutionDownloadFactKind::PromoteFailed,
    ExecutionDownloadFactKind::ProviderFailure,
    ExecutionDownloadFactKind::SizeMismatch,
    ExecutionDownloadFactKind::TempDiscarded,
    ExecutionDownloadFactKind::TempWriteFailed,
    ExecutionDownloadFactKind::WrittenToTemp,
    ExecutionDownloadFactKind::Promoted,
];

fn execution_download_fact_kind_index(kind: ExecutionDownloadFactKind) -> usize {
    EXECUTION_DOWNLOAD_FACT_KINDS
        .iter()
        .position(|candidate| *candidate == kind)
        .expect("every execution download fact kind has a bounded journal slot")
}

async fn reconcile_install_journal_error(
    journals: &OperationJournalStore,
    operation_id: &OperationId,
    error: OperationJournalStoreError,
    expected: impl Fn(&OperationJournalEntry) -> bool,
) -> Result<InstallJournalReconciliation, OperationJournalStoreError> {
    reconcile_install_journal_transition(journals, operation_id, error, expected).await
}

pub(crate) async fn begin_content_operation_journal_for_session(
    journals: &OperationJournalStore,
    operation_id: &OperationId,
    install_id: &str,
    instance_id: &str,
) -> Result<(), OperationJournalStoreError> {
    let expected = planned_content_journal_for_session(operation_id, install_id, instance_id);
    create_fresh_install_journal(journals, expected).await
}

pub(crate) async fn begin_install_operation_journal_for_session(
    journals: &OperationJournalStore,
    operation_id: &OperationId,
    install_id: &str,
    identity: &InstallJournalIdentity,
) -> Result<(), OperationJournalStoreError> {
    let expected = planned_install_journal_for_session(operation_id, install_id, identity);
    create_fresh_install_journal(journals, expected).await
}

async fn create_fresh_install_journal(
    journals: &OperationJournalStore,
    expected: OperationJournalEntry,
) -> Result<(), OperationJournalStoreError> {
    loop {
        match journals.create_fresh(expected.clone()).await {
            Err(OperationJournalStoreError::RetryRequired) => {
                if journals.retry().await.is_err() {
                    return Err(OperationJournalStoreError::AlreadyExists);
                }
            }
            result => return result,
        }
    }
}

pub(super) fn planned_content_journal_for_session(
    operation_id: &OperationId,
    install_id: &str,
    instance_id: &str,
) -> OperationJournalEntry {
    let mut entry = OperationJournalEntry::new(
        JournalId::new(format!("journal-{operation_id}")),
        operation_id.clone(),
        CommandKind::ModifyInstanceContent,
        StabilizationSystem::Application,
        OwnershipClass::LauncherManaged,
        RollbackState::NotApplicable,
    );
    entry.targets.push(install_session_target(install_id));
    entry.targets.push(content_instance_target(instance_id));
    entry.planned_steps.push(install_journal_step(
        "modify_instance_content",
        OperationPhase::Planning,
        OperationStepResult::Planned,
        None,
    ));
    entry
}

pub(super) fn planned_install_journal_for_session(
    operation_id: &OperationId,
    install_id: &str,
    identity: &InstallJournalIdentity,
) -> OperationJournalEntry {
    let mut entry = OperationJournalEntry::new(
        JournalId::new(format!("journal-{operation_id}")),
        operation_id.clone(),
        CommandKind::InstallVersion,
        StabilizationSystem::Application,
        OwnershipClass::LauncherManaged,
        RollbackState::NotApplicable,
    );
    entry.targets.push(install_session_target(install_id));
    entry
        .targets
        .push(install_version_target(identity.target_version_id()));
    let mut planned_step = install_journal_step(
        "install_version",
        OperationPhase::Planning,
        OperationStepResult::Planned,
        None,
    );
    planned_step.generated_facts = identity.planned_facts();
    entry.planned_steps.push(planned_step);
    entry
}

pub(crate) fn install_operation_journal_for_session(
    journals: &OperationJournalStore,
    install_id: &str,
) -> Option<OperationJournalEntry> {
    let session_target = install_session_target(install_id);
    journals
        .matching_entries(|entry| {
            matches!(
                entry.command,
                CommandKind::InstallVersion | CommandKind::ModifyInstanceContent
            ) && entry.targets.contains(&session_target)
        })
        .into_iter()
        .max_by_key(|entry| entry.sequence)
}

#[cfg(test)]
pub(crate) fn test_operation_id(seed: impl AsRef<[u8]>) -> OperationId {
    OperationId::deterministic_test(seed)
}

#[cfg(test)]
pub(crate) async fn begin_install_operation_journal(
    journals: &OperationJournalStore,
    operation_id: &OperationId,
    version_id: &str,
) -> Result<(), OperationJournalStoreError> {
    let install_id = operation_id.to_string();
    begin_install_operation_journal_for_session(
        journals,
        operation_id,
        &install_id,
        &InstallJournalIdentity::vanilla(version_id),
    )
    .await
}

#[cfg(test)]
pub(crate) async fn begin_content_operation_journal(
    journals: &OperationJournalStore,
    operation_id: &OperationId,
    instance_id: &str,
) -> Result<(), OperationJournalStoreError> {
    let install_id = operation_id.to_string();
    begin_content_operation_journal_for_session(journals, operation_id, &install_id, instance_id)
        .await
}

#[cfg(test)]
pub(super) fn planned_install_journal(
    operation_id: &OperationId,
    version_id: &str,
) -> OperationJournalEntry {
    planned_install_journal_for_session(
        operation_id,
        &operation_id.to_string(),
        &InstallJournalIdentity::vanilla(version_id),
    )
}

pub async fn record_install_operation_progress(
    journals: &OperationJournalStore,
    operation_id: &OperationId,
    progress: &DownloadProgress,
    progress_journal: &mut InstallProgressJournalTracker,
) -> Result<(), OperationJournalStoreError> {
    record_operation_progress(
        journals,
        operation_id,
        CommandKind::InstallVersion,
        "install",
        progress,
        None,
        progress_journal,
        false,
    )
    .await
}

pub(super) async fn record_install_operation_progress_durably(
    journals: &OperationJournalStore,
    operation_id: &OperationId,
    progress: &DownloadProgress,
    progress_journal: &mut InstallProgressJournalTracker,
) -> Result<(), OperationJournalStoreError> {
    record_operation_progress(
        journals,
        operation_id,
        CommandKind::InstallVersion,
        "install",
        progress,
        None,
        progress_journal,
        true,
    )
    .await
}

pub async fn reconcile_install_operation_terminal(
    journals: &OperationJournalStore,
    operation_id: &OperationId,
    progress: &DownloadProgress,
) -> Result<DownloadProgress, OperationJournalStoreError> {
    debug_assert!(progress.done);
    if let Some(progress) = authoritative_install_terminal_progress(journals, operation_id) {
        return Ok(progress);
    }
    record_install_operation_progress(
        journals,
        operation_id,
        progress,
        &mut InstallProgressJournalTracker::default(),
    )
    .await?;
    Ok(sanitize_install_progress(progress.clone()))
}

pub(crate) async fn record_content_operation_progress(
    journals: &OperationJournalStore,
    operation_id: &OperationId,
    progress: &DownloadProgress,
    metrics: Option<&ContentDownloadMetrics>,
    progress_journal: &mut InstallProgressJournalTracker,
) -> Result<(), OperationJournalStoreError> {
    record_operation_progress(
        journals,
        operation_id,
        CommandKind::ModifyInstanceContent,
        "content",
        progress,
        metrics,
        progress_journal,
        false,
    )
    .await
}

async fn record_operation_progress(
    journals: &OperationJournalStore,
    operation_id: &OperationId,
    command: CommandKind,
    step_namespace: &str,
    progress: &DownloadProgress,
    terminal_metrics: Option<&ContentDownloadMetrics>,
    progress_journal: &mut InstallProgressJournalTracker,
    durable_nonterminal: bool,
) -> Result<(), OperationJournalStoreError> {
    let phase = safe_progress_phase(&progress.phase);
    let terminal = progress.done;
    if !terminal && progress_journal.contains(&phase) {
        return Ok(());
    }

    loop {
        let step_result = if terminal && progress.error.is_some() {
            OperationStepResult::Failed
        } else {
            OperationStepResult::Completed
        };
        let mut step = install_progress_step(step_namespace, &phase, step_result, progress);
        if terminal && let Some(metrics) = terminal_metrics {
            step.phase = OperationPhase::Downloading;
            step.set_metrics(OperationStepMetrics::ContentDownload(metrics.clone()));
        }
        let failure_point = terminal
            .then(|| {
                progress
                    .error
                    .as_ref()
                    .map(|_| format!("{step_namespace}_progress_{phase}"))
            })
            .flatten();
        let result = if terminal && progress.error.is_some() {
            if terminal_metrics.is_some() {
                journals
                    .record_failure_with_metrics(
                        operation_id,
                        step.clone(),
                        failure_point
                            .as_deref()
                            .expect("failed progress has failure point"),
                    )
                    .await
            } else {
                journals
                    .record_failure(
                        operation_id,
                        step.clone(),
                        failure_point
                            .as_deref()
                            .expect("failed progress has failure point"),
                        OperationOutcome::Failed,
                    )
                    .await
            }
        } else if terminal {
            if terminal_metrics.is_some() {
                journals
                    .record_success_with_metrics(operation_id, step.clone())
                    .await
            } else {
                journals
                    .record_success(operation_id, step.clone(), OperationOutcome::Succeeded)
                    .await
            }
        } else if durable_nonterminal {
            journals
                .record_idempotent_checkpoint(operation_id, step.clone())
                .await
        } else {
            journals.record_progress(operation_id, step.clone()).await
        };

        match result {
            Ok(()) => {
                if !terminal {
                    progress_journal.record(phase);
                }
                return Ok(());
            }
            Err(error) => {
                match reconcile_install_journal_error(journals, operation_id, error, |entry| {
                    install_progress_transition_matches(
                        entry,
                        operation_id,
                        command,
                        &step,
                        terminal,
                        failure_point.as_deref(),
                    )
                })
                .await?
                {
                    InstallJournalReconciliation::MutationCommitted => {
                        if !terminal {
                            progress_journal.record(phase);
                        }
                        return Ok(());
                    }
                    InstallJournalReconciliation::RetryMutation => {}
                }
            }
        }
    }
}

pub async fn record_install_operation_interrupted(
    journals: &OperationJournalStore,
    operation_id: &OperationId,
    progress: &DownloadProgress,
) -> Result<(), OperationJournalStoreError> {
    let phase = safe_progress_phase(&progress.phase);
    let evidence = install_execution_fact(
        operation_id,
        "install_worker_interrupted",
        OwnershipClass::LauncherManaged,
        ExecutionFactKind::DownloadNetworkFailure,
        [("phase", phase.as_str())],
    );
    record_operation_interrupted(
        journals,
        operation_id,
        CommandKind::InstallVersion,
        "install",
        progress,
        None,
        &[evidence],
    )
    .await
}

pub(crate) async fn record_content_operation_interrupted(
    journals: &OperationJournalStore,
    operation_id: &OperationId,
    progress: &DownloadProgress,
    metrics: &ContentDownloadMetrics,
    execution_facts: &[ExecutionDownloadFact],
) -> Result<(), OperationJournalStoreError> {
    let evidence = install_failure_evidence_from_download_facts(operation_id, execution_facts);
    record_operation_interrupted(
        journals,
        operation_id,
        CommandKind::ModifyInstanceContent,
        "content",
        progress,
        Some(metrics),
        &evidence,
    )
    .await
}

async fn record_operation_interrupted(
    journals: &OperationJournalStore,
    operation_id: &OperationId,
    command: CommandKind,
    step_namespace: &str,
    progress: &DownloadProgress,
    terminal_metrics: Option<&ContentDownloadMetrics>,
    evidence: &[ExecutionFact],
) -> Result<(), OperationJournalStoreError> {
    let durable = if evidence.is_empty() {
        None
    } else {
        let memory_window =
            ProviderFailureObservationWindow::from_observed_at(&journals.now_timestamp())
                .ok_or(OperationJournalStoreError::InvalidGuardianOutcome)?;
        let evidence =
            operation_evidence_batch(operation_id, OperationPhase::Downloading, evidence)?;
        Some(
            assess_install_guardian_failure(None, &evidence, OperationPhase::Downloading)
                .as_ref()
                .and_then(|assessment| {
                    install_guardian_terminal_update(assessment, &evidence, &memory_window)
                })
                .ok_or(OperationJournalStoreError::InvalidGuardianOutcome)?,
        )
    };
    let mut step = install_progress_step(
        step_namespace,
        &safe_progress_phase(&progress.phase),
        OperationStepResult::Failed,
        progress,
    );
    if let Some(metrics) = terminal_metrics {
        step.phase = OperationPhase::Downloading;
        step.set_metrics(OperationStepMetrics::ContentDownload(metrics.clone()));
    }
    let failure_point = format!("{step_namespace}_worker_interrupted");
    loop {
        if journals.get(operation_id).as_ref().is_some_and(|entry| {
            durable.as_ref().map_or_else(
                || {
                    install_progress_transition_matches(
                        entry,
                        operation_id,
                        command,
                        &step,
                        true,
                        Some(&failure_point),
                    )
                },
                |durable| {
                    install_failure_with_evidence_matches(
                        entry,
                        operation_id,
                        command,
                        &step,
                        &failure_point,
                        durable,
                    )
                },
            )
        }) {
            return Ok(());
        }
        let result = if let Some(durable) = &durable {
            journals
                .record_failure_with_guardian_evidence(
                    step.clone(),
                    failure_point.clone(),
                    OperationOutcome::Failed,
                    durable.clone(),
                )
                .await
        } else if terminal_metrics.is_some() {
            journals
                .record_failure_with_metrics(operation_id, step.clone(), failure_point.clone())
                .await
        } else {
            journals
                .record_failure(
                    operation_id,
                    step.clone(),
                    failure_point.clone(),
                    OperationOutcome::Failed,
                )
                .await
        };
        match result {
            Ok(()) => return Ok(()),
            Err(error) => {
                match reconcile_install_journal_error(journals, operation_id, error, |entry| {
                    durable.as_ref().map_or_else(
                        || {
                            install_progress_transition_matches(
                                entry,
                                operation_id,
                                command,
                                &step,
                                true,
                                Some(&failure_point),
                            )
                        },
                        |durable| {
                            install_failure_with_evidence_matches(
                                entry,
                                operation_id,
                                command,
                                &step,
                                &failure_point,
                                durable,
                            )
                        },
                    )
                })
                .await?
                {
                    InstallJournalReconciliation::MutationCommitted => return Ok(()),
                    InstallJournalReconciliation::RetryMutation => {}
                }
            }
        }
    }
}

pub(crate) async fn record_content_operation_initialization_cancelled(
    journals: &OperationJournalStore,
    operation_id: &OperationId,
) -> Result<(), OperationJournalStoreError> {
    let progress = DownloadProgress {
        phase: "initializing".to_string(),
        current: 0,
        total: 1,
        file: None,
        error: Some("content operation stopped before initialization completed".to_string()),
        done: true,
        bytes_done: None,
        bytes_total: None,
    };
    let step = install_progress_step(
        "content",
        "initializing",
        OperationStepResult::Failed,
        &progress,
    );
    loop {
        if journals.get(operation_id).as_ref().is_some_and(|entry| {
            install_progress_transition_matches(
                entry,
                operation_id,
                CommandKind::ModifyInstanceContent,
                &step,
                true,
                Some("content_initialization_cancelled"),
            )
        }) {
            return Ok(());
        }
        match journals
            .record_failure(
                operation_id,
                step.clone(),
                "content_initialization_cancelled",
                OperationOutcome::Failed,
            )
            .await
        {
            Ok(()) => return Ok(()),
            Err(error) => {
                match reconcile_install_journal_error(journals, operation_id, error, |entry| {
                    install_progress_transition_matches(
                        entry,
                        operation_id,
                        CommandKind::ModifyInstanceContent,
                        &step,
                        true,
                        Some("content_initialization_cancelled"),
                    )
                })
                .await?
                {
                    InstallJournalReconciliation::MutationCommitted => return Ok(()),
                    InstallJournalReconciliation::RetryMutation => {}
                }
            }
        }
    }
}

pub(crate) fn content_terminal_progress_is_visible(
    entry: &OperationJournalEntry,
    operation_id: &OperationId,
    progress: &DownloadProgress,
    metrics: &ContentDownloadMetrics,
) -> bool {
    let phase = safe_progress_phase(&progress.phase);
    let mut step = install_progress_step(
        "content",
        &phase,
        if progress.error.is_some() {
            OperationStepResult::Failed
        } else {
            OperationStepResult::Completed
        },
        progress,
    );
    step.phase = OperationPhase::Downloading;
    step.set_metrics(OperationStepMetrics::ContentDownload(metrics.clone()));
    let failure_point = progress
        .error
        .as_ref()
        .map(|_| format!("content_progress_{phase}"));
    install_progress_transition_matches(
        entry,
        operation_id,
        CommandKind::ModifyInstanceContent,
        &step,
        true,
        failure_point.as_deref(),
    )
}

pub(super) async fn record_install_operation_initialization_cancelled(
    journals: &OperationJournalStore,
    operation_id: &OperationId,
) -> Result<(), OperationJournalStoreError> {
    let progress = interrupted_install_progress();
    let evidence = install_execution_fact(
        operation_id,
        "install_initialization_cancelled",
        OwnershipClass::LauncherManaged,
        ExecutionFactKind::DownloadNetworkFailure,
        [("phase", "initializing")],
    );
    let memory_window =
        ProviderFailureObservationWindow::from_observed_at(&journals.now_timestamp())
            .ok_or(OperationJournalStoreError::InvalidGuardianOutcome)?;
    let evidence_batch = operation_evidence_batch(
        operation_id,
        OperationPhase::Downloading,
        std::slice::from_ref(&evidence),
    )?;
    let durable =
        assess_install_guardian_failure(None, &evidence_batch, OperationPhase::Downloading)
            .as_ref()
            .and_then(|assessment| {
                install_guardian_terminal_update(assessment, &evidence_batch, &memory_window)
            })
            .ok_or(OperationJournalStoreError::InvalidGuardianOutcome)?;
    let step = install_progress_step(
        "install",
        "initializing",
        OperationStepResult::Failed,
        &progress,
    );
    loop {
        match journals
            .record_failure_with_guardian_evidence(
                step.clone(),
                "install_initialization_cancelled",
                OperationOutcome::Failed,
                durable.clone(),
            )
            .await
        {
            Ok(()) => return Ok(()),
            Err(error) => {
                match reconcile_install_journal_error(journals, operation_id, error, |entry| {
                    install_failure_with_evidence_matches(
                        entry,
                        operation_id,
                        CommandKind::InstallVersion,
                        &step,
                        "install_initialization_cancelled",
                        &durable,
                    )
                })
                .await?
                {
                    InstallJournalReconciliation::MutationCommitted => return Ok(()),
                    InstallJournalReconciliation::RetryMutation => {}
                }
            }
        }
    }
}

pub(super) async fn record_loader_install_operation_guardian_failure_outcome(
    producer: &ProducerLease,
    journals: Arc<OperationJournalStore>,
    failure_memory: Arc<GuardianFailureMemoryStore>,
    operation_id: &OperationId,
    target_id: &str,
    failure: &LoaderActiveInstallFailure,
) -> Result<(), OperationJournalStoreError> {
    let failure_kind = failure.kind();
    let (kind, ownership, phase) = loader_install_guardian_evidence_kind(failure_kind);
    let evidence = loader_error_guardian_failure_evidence(
        operation_id,
        target_id,
        failure,
        failure_kind,
        kind,
        ownership,
    );
    record_install_guardian_failure_outcome(
        producer,
        journals,
        failure_memory,
        operation_id,
        &[evidence],
        phase,
    )
    .await
}

pub(super) async fn record_loader_base_install_dependency_guardian_failure_outcome(
    journals: &OperationJournalStore,
    operation_id: &OperationId,
    target_id: &str,
    base_version_id: &str,
) -> Result<(), OperationJournalStoreError> {
    let evidence = install_execution_fact(
        operation_id,
        target_id,
        OwnershipClass::LauncherManaged,
        ExecutionFactKind::InstallDependencyFailed,
        [
            ("dependency", "base_version"),
            ("base_version", base_version_id),
        ],
    );
    record_install_guardian_failure_outcome_without_memory(
        journals,
        operation_id,
        &[evidence],
        OperationPhase::Downloading,
    )
    .await
}

pub fn install_guardian_outcome_summary_from_journal(
    entry: &OperationJournalEntry,
) -> Option<crate::guardian::GuardianInstallOutcomeSummary> {
    let terminal = entry.guardian_install_terminal()?;
    guardian_install_outcome_from_terminal(terminal.diagnosis_id(), terminal.action())
}

pub(super) async fn settle_startup_install_guardian_failure_memory(
    journals: &OperationJournalStore,
    failure_memory: &GuardianFailureMemoryStore,
) -> Result<(), OperationJournalStoreError> {
    let _settlement = failure_memory.lock_install_guardian_settlement().await;
    let mut pending_retries = 0;
    settle_startup_install_guardian_pending(failure_memory, &mut pending_retries).await?;

    let mut active_keys = BTreeSet::new();
    let mut candidates = Vec::new();
    for entry in journals.matching_entries(|entry| {
        matches!(
            entry.command,
            CommandKind::InstallVersion | CommandKind::ModifyInstanceContent
        )
    }) {
        let Some(terminal) = entry.guardian_install_terminal() else {
            continue;
        };
        if terminal.action() != GuardianActionKind::Retry {
            continue;
        }
        if !install_journal_identity_matches(&entry, &entry.operation_id, entry.command)
            || !install_retry_carrier_journal_state_is_valid(&entry)
            || terminal.diagnosis_id() != DiagnosisId::DownloadUnavailable
        {
            return Err(OperationJournalStoreError::InvalidGuardianOutcome);
        }
        let Some(durable_memory) = terminal.memory() else {
            return Err(OperationJournalStoreError::InvalidGuardianOutcome);
        };
        let memory = GuardianInstallOutcomeMemoryPersistence::from_durable(durable_memory);
        if !install_provider_retry_target_is_valid(memory.target()) {
            return Err(OperationJournalStoreError::InvalidGuardianOutcome);
        }
        let candidate = GuardianFailureMemoryEntry::observed(
            DiagnosisId::DownloadUnavailable,
            GuardianDomain::Download,
            memory.target().clone(),
            GuardianMode::Managed,
            Some(PROVIDER_FAILURE_MEMORY_SOURCE),
            memory.observed_at().to_string(),
        )
        .with_action(
            GuardianActionKind::Retry,
            FailureMemoryActionOutcome::Retried,
        )
        .with_suppression_until(memory.suppression_until().to_string());
        if !memory.binding_matches_target(&candidate.key)
            || !active_keys.insert(candidate.key.as_str().to_string())
        {
            return Err(OperationJournalStoreError::InvalidGuardianOutcome);
        }
        let candidate = match failure_memory.construct_entry(candidate) {
            Ok(candidate) => candidate,
            Err(FailureMemoryStoreError::Expired) => continue,
            Err(_) => return Err(OperationJournalStoreError::InvalidGuardianOutcome),
        };
        candidates.push(candidate);
    }

    settle_startup_install_guardian_retry_batch(failure_memory, candidates).await
}

fn install_retry_carrier_journal_state_is_valid(entry: &OperationJournalEntry) -> bool {
    match (entry.status, entry.outcome) {
        (OperationStatus::Planned | OperationStatus::Running, None) => {
            entry.failure_point.is_none()
        }
        (OperationStatus::Failed, Some(OperationOutcome::Failed)) => entry.failure_point.is_some(),
        _ => false,
    }
}

fn install_provider_retry_target_is_valid(target: &TargetDescriptor) -> bool {
    target.system == StabilizationSystem::Execution
        && target.kind == TargetKind::Artifact
        && matches!(
            target.ownership,
            OwnershipClass::LauncherManaged | OwnershipClass::ExternalProviderDerived
        )
}

async fn settle_startup_install_guardian_pending(
    failure_memory: &GuardianFailureMemoryStore,
    retries: &mut usize,
) -> Result<(), OperationJournalStoreError> {
    loop {
        match failure_memory.settle_install_guardian_pending().await {
            Ok(()) => return Ok(()),
            Err(error)
                if retry_install_guardian_memory_persistence(
                    &error,
                    retries,
                    "startup_pending",
                )
                .await => {}
            Err(_) => {
                return Err(OperationJournalStoreError::GuardianFailureMemoryUnavailable);
            }
        }
    }
}

async fn settle_startup_install_guardian_retry_batch(
    failure_memory: &GuardianFailureMemoryStore,
    entries: Vec<GuardianFailureMemoryEntry>,
) -> Result<(), OperationJournalStoreError> {
    let mut retries = 0;
    loop {
        match failure_memory
            .reconcile_install_guardian_retry_batch(entries.clone())
            .await
        {
            Ok(()) => return Ok(()),
            Err(error @ FailureMemoryStoreError::Validation(_)) => {
                warn!(
                    failure_memory_error = error.class(),
                    "Guardian startup Retry carrier conflicts with failure memory"
                );
                return Err(OperationJournalStoreError::InvalidGuardianOutcome);
            }
            Err(error)
                if retry_install_guardian_memory_persistence(
                    &error,
                    &mut retries,
                    "startup_publication",
                )
                .await =>
            {
                settle_startup_install_guardian_pending(failure_memory, &mut retries).await?;
            }
            Err(_) => {
                return Err(OperationJournalStoreError::GuardianFailureMemoryUnavailable);
            }
        }
    }
}

pub fn sanitize_install_progress(mut progress: DownloadProgress) -> DownloadProgress {
    progress.phase = sanitize_evidence_token(&progress.phase, RedactionAudience::UserVisible, 48)
        .unwrap_or_else(|| "install".to_string());
    progress.file = progress
        .file
        .take()
        .and_then(|file| sanitize_evidence_token(&file, RedactionAudience::UserVisible, 96));
    progress.error = progress.error.take().map(|error| {
        if progress.done {
            if is_specific_terminal_install_failure_message(&error) {
                return error;
            }
            return INSTALL_FAILURE_MESSAGE.to_string();
        }
        sanitize_public_diagnostic_text(
            &error,
            RedactionAudience::UserVisible,
            160,
            INSTALL_FAILURE_MESSAGE,
        )
    });
    progress
}

pub(crate) fn install_progress_with_terminal_error(
    mut progress: DownloadProgress,
    error: &DownloadError,
) -> DownloadProgress {
    if progress.done
        && progress.error.is_some()
        && let Some(message) = specific_terminal_install_failure_message(error)
    {
        progress.error = Some(message);
    }
    progress
}

fn specific_terminal_install_failure_message(error: &DownloadError) -> Option<String> {
    match error {
        DownloadError::RuntimeUnavailableForPlatform {
            component,
            platform,
        } => Some(runtime_unavailable_install_failure_message(
            component, platform,
        )),
        DownloadError::RuntimeRosettaRequired { component } => {
            Some(runtime_rosetta_required_install_failure_message(component))
        }
        _ => None,
    }
}

fn runtime_unavailable_install_failure_message(component: &str, platform: &str) -> String {
    let component = sanitize_evidence_token(component, RedactionAudience::UserVisible, 64)
        .unwrap_or_else(|| "required-runtime".to_string());
    let platform = sanitize_evidence_token(platform, RedactionAudience::UserVisible, 64)
        .unwrap_or_else(|| "this-device".to_string());
    format!(
        "{RUNTIME_UNAVAILABLE_INSTALL_FAILURE_MESSAGE_PREFIX} Required runtime: {component} on {platform}."
    )
}

fn runtime_rosetta_required_install_failure_message(component: &str) -> String {
    let component = sanitize_evidence_token(component, RedactionAudience::UserVisible, 64)
        .unwrap_or_else(|| "required-runtime".to_string());
    format!(
        "{RUNTIME_ROSETTA_REQUIRED_INSTALL_FAILURE_MESSAGE_PREFIX} Required runtime: {component}. Install Rosetta 2 by running `{ROSETTA_INSTALL_COMMAND}` in Terminal, then retry."
    )
}

fn is_specific_terminal_install_failure_message(error: &str) -> bool {
    if error.starts_with(RUNTIME_ROSETTA_REQUIRED_INSTALL_FAILURE_MESSAGE_PREFIX) {
        return is_rosetta_required_terminal_install_failure_message(error);
    }

    error.starts_with(RUNTIME_UNAVAILABLE_INSTALL_FAILURE_MESSAGE_PREFIX)
        && sanitize_public_diagnostic_text(
            error,
            RedactionAudience::UserVisible,
            220,
            INSTALL_FAILURE_MESSAGE,
        )
        .as_str()
            == error
}

fn is_rosetta_required_terminal_install_failure_message(error: &str) -> bool {
    let prefix =
        format!("{RUNTIME_ROSETTA_REQUIRED_INSTALL_FAILURE_MESSAGE_PREFIX} Required runtime: ");
    let suffix = format!(". {ROSETTA_REQUIRED_INSTALL_GUIDANCE}");
    let Some(component) = error
        .strip_prefix(&prefix)
        .and_then(|rest| rest.strip_suffix(&suffix))
    else {
        return false;
    };

    sanitize_evidence_token(component, RedactionAudience::UserVisible, 64)
        .is_some_and(|sanitized| sanitized == component)
}

pub(crate) fn vanilla_install_progress_view_model(
    progress: &DownloadProgress,
) -> InstallProgressViewModel {
    install_progress_view_model(progress, InstallProgressKind::Vanilla)
}

#[cfg(test)]
pub(crate) fn loader_install_progress_view_model(
    progress: &DownloadProgress,
) -> InstallProgressViewModel {
    install_progress_view_model(progress, InstallProgressKind::Loader)
}

pub fn public_vanilla_install_progress_record_json(record: &InstallProgressRecord) -> Value {
    public_install_progress_record_json(record, InstallProgressKind::Vanilla)
}

pub fn public_loader_install_progress_record_json(record: &InstallProgressRecord) -> Value {
    public_install_progress_record_json(record, InstallProgressKind::Loader)
}

pub(crate) fn vanilla_install_progress_record_view_model(
    record: &InstallProgressRecord,
) -> InstallProgressViewModel {
    install_progress_record_view_model(record, InstallProgressKind::Vanilla)
}

pub(crate) fn loader_install_progress_record_view_model(
    record: &InstallProgressRecord,
) -> InstallProgressViewModel {
    install_progress_record_view_model(record, InstallProgressKind::Loader)
}

#[derive(Default)]
pub(crate) struct InstallProgressPresenter {
    high_watermarks: [u8; 2],
    complete_denominator_seen: bool,
}

impl InstallProgressPresenter {
    pub(crate) fn record(&mut self, progress: DownloadProgress) -> InstallProgressRecord {
        self.complete_denominator_seen |= progress.bytes_total.is_some_and(|total| total > 0);
        let vanilla = self.public_json(&progress, InstallProgressKind::Vanilla);
        let loader = self.public_json(&progress, InstallProgressKind::Loader);
        let vanilla_event_json =
            serde_json::to_string(&vanilla).unwrap_or_else(|_| "{}".to_string());
        let loader_event_json = serde_json::to_string(&loader).unwrap_or_else(|_| "{}".to_string());
        InstallProgressRecord::with_event_json(progress, vanilla_event_json, loader_event_json)
    }

    fn public_json(&mut self, progress: &DownloadProgress, kind: InstallProgressKind) -> Value {
        let mut view_model = install_progress_view_model(progress, kind);
        if !view_model.terminal {
            if !self.complete_denominator_seen {
                view_model.progress_pct = view_model.progress_pct.min(kind.pre_transfer_ceiling());
            }
            let high_watermark = &mut self.high_watermarks[kind.index()];
            view_model.progress_pct = view_model.progress_pct.max(*high_watermark);
            *high_watermark = view_model.progress_pct;
        }
        public_install_progress_json_with_view_model(progress, view_model)
    }
}

#[derive(Default)]
pub(crate) struct InstallProgressCoalescer {
    last_emitted: Option<DownloadProgress>,
    pending: Option<DownloadProgress>,
    pending_count: usize,
}

impl InstallProgressCoalescer {
    pub(crate) fn push(&mut self, progress: DownloadProgress) -> Vec<DownloadProgress> {
        if should_passthrough_progress(&progress) || self.is_phase_transition(&progress) {
            let mut emitted = self.flush_vec();
            emitted.push(self.mark_emitted(progress));
            return emitted;
        }

        if self.should_emit_coalesced_now(&progress) {
            self.pending = None;
            self.pending_count = 0;
            return vec![self.mark_emitted(progress)];
        }

        self.pending = Some(progress);
        self.pending_count = self.pending_count.saturating_add(1);
        if self.pending_count >= COALESCED_PROGRESS_EVENT_INTERVAL {
            return self.flush_vec();
        }

        Vec::new()
    }

    pub(crate) fn flush(&mut self) -> Option<DownloadProgress> {
        let progress = self.pending.take()?;
        self.pending_count = 0;
        Some(self.mark_emitted(progress))
    }

    fn flush_vec(&mut self) -> Vec<DownloadProgress> {
        self.flush().into_iter().collect()
    }

    fn mark_emitted(&mut self, progress: DownloadProgress) -> DownloadProgress {
        self.last_emitted = Some(progress.clone());
        progress
    }

    fn is_phase_transition(&self, progress: &DownloadProgress) -> bool {
        self.pending
            .as_ref()
            .or(self.last_emitted.as_ref())
            .is_some_and(|previous| previous.phase != progress.phase)
    }

    fn should_emit_coalesced_now(&self, progress: &DownloadProgress) -> bool {
        let Some(last) = self.last_emitted.as_ref() else {
            return true;
        };
        progress.bytes_total != last.bytes_total
            || progress.total != last.total
            || byte_progress_bucket(progress) != byte_progress_bucket(last)
            || (progress.total > 0 && progress.current >= progress.total)
    }
}

fn should_passthrough_progress(progress: &DownloadProgress) -> bool {
    progress.done
        || progress.error.is_some()
        || matches!(
            progress.phase.as_str(),
            "done" | "error" | "java_runtime_ready"
        )
        || !is_coalesced_progress_phase(progress.phase.as_str())
}

fn is_coalesced_progress_phase(phase: &str) -> bool {
    matches!(phase, "libraries" | "loader_libraries" | "java_runtime")
}

fn byte_progress_bucket(progress: &DownloadProgress) -> Option<u8> {
    let (done, total) = (progress.bytes_done?, progress.bytes_total?);
    if total == 0 {
        return None;
    }
    Some((((u128::from(done.min(total)) * 100) / u128::from(total)).min(100)) as u8)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum InstallProgressKind {
    Vanilla,
    Loader,
}

impl InstallProgressKind {
    const fn index(self) -> usize {
        match self {
            Self::Vanilla => 0,
            Self::Loader => 1,
        }
    }

    const fn pre_transfer_ceiling(self) -> u8 {
        match self {
            Self::Vanilla => 2,
            Self::Loader => 8,
        }
    }
}

fn public_install_progress_json(progress: &DownloadProgress, kind: InstallProgressKind) -> Value {
    let view_model = install_progress_view_model(progress, kind);
    public_install_progress_json_with_view_model(progress, view_model)
}

fn public_install_progress_record_json(
    record: &InstallProgressRecord,
    kind: InstallProgressKind,
) -> Value {
    record
        .event_json(kind == InstallProgressKind::Loader)
        .and_then(|payload| serde_json::from_str(payload).ok())
        .unwrap_or_else(|| public_install_progress_json(&record.progress, kind))
}

fn install_progress_record_view_model(
    record: &InstallProgressRecord,
    kind: InstallProgressKind,
) -> InstallProgressViewModel {
    let payload = public_install_progress_record_json(record, kind);
    serde_json::from_value(payload["view_model"].clone())
        .unwrap_or_else(|_| install_progress_view_model(&record.progress, kind))
}

fn public_install_progress_json_with_view_model(
    progress: &DownloadProgress,
    view_model: InstallProgressViewModel,
) -> Value {
    let progress = sanitize_install_progress(progress.clone());
    let mut payload = serde_json::to_value(&progress).unwrap_or_else(|_| json!({}));
    payload["view_model"] = json!(view_model);
    payload
}

fn install_progress_view_model(
    progress: &DownloadProgress,
    kind: InstallProgressKind,
) -> InstallProgressViewModel {
    let progress = sanitize_install_progress(progress.clone());
    let phase = progress.phase.trim();
    let label = install_progress_label(&progress, kind);
    let failed = phase == "error" || progress.error.is_some();
    let terminal = progress.done || failed;
    InstallProgressViewModel {
        phase_id: if phase.is_empty() {
            "install".to_string()
        } else {
            phase.to_string()
        },
        progress_pct: install_progress_pct(&progress, kind),
        active_step: install_active_step_view_model(&progress, &label),
        label,
        terminal,
        failed,
    }
}

fn install_progress_label(progress: &DownloadProgress, kind: InstallProgressKind) -> String {
    match progress.phase.as_str() {
        "profile" => progress
            .file
            .clone()
            .unwrap_or_else(|| "Preparing loader profile".to_string()),
        "artifacts" => progress
            .file
            .clone()
            .unwrap_or_else(|| "Downloading loader artifacts".to_string()),
        "loader_libraries" => count_label("Loader libraries", progress),
        "processors" => progress
            .file
            .clone()
            .unwrap_or_else(|| count_label("Running processors", progress)),
        "loader_overlay" => "Applying loader archive".to_string(),
        "loader_publish" => "Publishing loader version".to_string(),
        "version_json" => {
            if progress.current >= progress.total && progress.total > 0 {
                "Version info ready".to_string()
            } else {
                "Resolving version info".to_string()
            }
        }
        "client_jar" => "Downloading game JAR".to_string(),
        "libraries" => count_label("Libraries", progress),
        "asset_index" => {
            if progress.current >= progress.total && progress.total > 0 {
                "Asset index ready".to_string()
            } else {
                "Fetching asset index".to_string()
            }
        }
        "assets" => count_label("Assets", progress),
        "log_config" => "Downloading log config".to_string(),
        "java_runtime" => java_runtime_label(progress),
        "java_runtime_ready" => "Java runtime ready".to_string(),
        "planning" => "Checking content".to_string(),
        "download" => count_label("Downloading content", progress),
        "overrides" => "Applying pack configuration".to_string(),
        "commit" => "Finishing content changes".to_string(),
        "removing" => "Removing content".to_string(),
        "recovering" => "Guardian is verifying install state".to_string(),
        "done" => "Complete".to_string(),
        "error" | "error_instance_removed" => progress
            .error
            .clone()
            .unwrap_or_else(|| INSTALL_FAILURE_MESSAGE.to_string()),
        phase => progress.file.clone().unwrap_or_else(|| match kind {
            InstallProgressKind::Loader => {
                if phase.is_empty() {
                    "Working on loader install".to_string()
                } else {
                    format!("Working on {phase}")
                }
            }
            InstallProgressKind::Vanilla => {
                if phase.is_empty() {
                    "Working on install".to_string()
                } else {
                    format!("Working on {phase}")
                }
            }
        }),
    }
}

fn install_progress_pct(progress: &DownloadProgress, kind: InstallProgressKind) -> u8 {
    if let Some(pct) = byte_weighted_install_pct(progress, kind) {
        return pct;
    }
    // Fallback for events without transfer-plan facts: pre-plan phases,
    // loader-specific work, and journal-replayed history.
    let pct = match (kind, progress.phase.as_str()) {
        (_, "done") => 100,
        (_, "error" | "error_instance_removed") => 100,
        (_, "planning") => 3,
        (_, "download") => 5 + (progress_fraction(progress) * 85.0).round() as i32,
        (_, "overrides") => 92,
        (_, "commit") => 96,
        (_, "removing") => 50,
        (InstallProgressKind::Vanilla, "version_json") => 2,
        (InstallProgressKind::Vanilla, "client_jar") => 7,
        (InstallProgressKind::Vanilla, "libraries") => {
            7 + (progress_fraction(progress) * 13.0).round() as i32
        }
        (InstallProgressKind::Vanilla, "asset_index") => 21,
        (InstallProgressKind::Vanilla, "assets") => {
            21 + (progress_fraction(progress) * 72.0).round() as i32
        }
        (InstallProgressKind::Vanilla, "log_config") => 94,
        (InstallProgressKind::Loader, "artifacts") => 5,
        (InstallProgressKind::Loader, "profile") => 82,
        (InstallProgressKind::Loader, "loader_libraries") => {
            82 + (progress_fraction(progress) * 8.0).round() as i32
        }
        (InstallProgressKind::Loader, "processors") => {
            90 + (progress_fraction(progress) * 9.0).round() as i32
        }
        (InstallProgressKind::Loader, "loader_overlay") => {
            82 + (progress_fraction(progress) * 15.0).round() as i32
        }
        (InstallProgressKind::Loader, "loader_publish") => 99,
        (InstallProgressKind::Loader, "version_json") => 8,
        (InstallProgressKind::Loader, "client_jar") => 12,
        (InstallProgressKind::Loader, "libraries") => {
            12 + (progress_fraction(progress) * 12.0).round() as i32
        }
        (InstallProgressKind::Loader, "asset_index") => 25,
        (InstallProgressKind::Loader, "assets") => {
            25 + (progress_fraction(progress) * 50.0).round() as i32
        }
        (InstallProgressKind::Loader, "log_config") => 76,
        _ => 0,
    };
    pct.clamp(0, 100) as u8
}

/// Overall progress from the installer's transfer-plan facts: bytes of
/// planned work completed across every concurrent phase (client jar,
/// libraries, assets, managed Java runtime). Capped below 100 so only the
/// terminal `done` event completes the bar. Loader installs reserve an early
/// provider span and a post-base span for phases that carry no byte facts.
fn byte_weighted_install_pct(progress: &DownloadProgress, kind: InstallProgressKind) -> Option<u8> {
    if matches!(progress.phase.as_str(), "done" | "error") {
        return None;
    }
    let (done, total) = (progress.bytes_done?, progress.bytes_total?);
    if total == 0 {
        return None;
    }
    let fraction = (done.min(total) as f64) / (total as f64);
    let (base, span) = match kind {
        InstallProgressKind::Vanilla => (0.0, 99.0),
        InstallProgressKind::Loader => (8.0, 72.0),
    };
    Some((base + fraction * span).round().clamp(0.0, 99.0) as u8)
}

fn install_active_step_view_model(
    progress: &DownloadProgress,
    label: &str,
) -> Option<InstallProgressStepViewModel> {
    if !matches!(
        progress.phase.as_str(),
        "java_runtime" | "processors" | "download"
    ) {
        return None;
    }
    if progress.total <= 0 {
        return None;
    }

    Some(InstallProgressStepViewModel {
        phase_id: progress.phase.clone(),
        label: label.to_string(),
        progress_pct: (progress_fraction(progress) * 100.0)
            .round()
            .clamp(0.0, 100.0) as u8,
        current: progress.current.max(0),
        total: progress.total,
    })
}

fn count_label(base: &str, progress: &DownloadProgress) -> String {
    if progress.total > 0 {
        format!("{} ({}/{})", base, progress.current.max(0), progress.total)
    } else {
        base.to_string()
    }
}

fn java_runtime_label(progress: &DownloadProgress) -> String {
    if progress.total > 0 {
        count_label("Java runtime files", progress)
    } else {
        "Preparing Java runtime".to_string()
    }
}

fn progress_fraction(progress: &DownloadProgress) -> f32 {
    if progress.total <= 0 {
        return 0.0;
    }
    (progress.current.max(0) as f32 / progress.total as f32).clamp(0.0, 1.0)
}

pub(super) fn install_failure_evidence_from_download_facts(
    operation_id: &OperationId,
    facts: &[ExecutionDownloadFact],
) -> Vec<ExecutionFact> {
    facts
        .iter()
        .map(|fact| execution_fact_from_download_fact(operation_id, fact))
        .collect()
}

pub(crate) struct ContentFailureOutcomeRequest<'a> {
    pub(crate) operation_id: &'a OperationId,
    pub(crate) download_facts: &'a [ExecutionDownloadFact],
    pub(crate) additional_evidence: Option<ExecutionFact>,
    pub(crate) phase: OperationPhase,
    pub(crate) terminal_progress: &'a DownloadProgress,
    pub(crate) metrics: &'a ContentDownloadMetrics,
}

pub(crate) async fn record_content_failure_outcome(
    producer: &ProducerLease,
    journals: Arc<OperationJournalStore>,
    failure_memory: Arc<GuardianFailureMemoryStore>,
    request: ContentFailureOutcomeRequest<'_>,
) -> Result<(), OperationJournalStoreError> {
    let ContentFailureOutcomeRequest {
        operation_id,
        download_facts,
        additional_evidence,
        phase,
        terminal_progress,
        metrics,
    } = request;
    let mut evidence = install_failure_evidence_from_download_facts(operation_id, download_facts);
    if let Some(additional_evidence) = additional_evidence {
        evidence.push(additional_evidence);
    }
    record_operation_guardian_failure_outcome(
        producer,
        journals,
        failure_memory,
        OperationGuardianFailureRequest {
            operation_id,
            command: CommandKind::ModifyInstanceContent,
            evidence: &evidence,
            phase,
            terminal: operation_guardian_failure_terminal(
                "content",
                terminal_progress,
                Some(metrics),
            ),
        },
    )
    .await
}

pub(super) fn install_failure_evidence_from_download_error_or_facts(
    operation_id: &OperationId,
    error: &DownloadError,
    facts: &[ExecutionDownloadFact],
) -> Vec<ExecutionFact> {
    if matches!(error, DownloadError::PublicationIndeterminate(_)) {
        return Vec::new();
    }
    if let Some(evidence) = typed_runtime_failure_evidence(operation_id, error) {
        return vec![evidence];
    }

    let terminal_facts = terminal_download_failure_facts_for_error(error, facts);
    let terminal_fact_evidence =
        install_failure_evidence_from_download_facts(operation_id, &terminal_facts);
    if should_prefer_terminal_download_facts(error) && !terminal_fact_evidence.is_empty() {
        return terminal_fact_evidence;
    }

    if let Some(evidence) = install_failure_evidence_from_download_error(operation_id, error) {
        return vec![evidence];
    }

    if !terminal_fact_evidence.is_empty() {
        return terminal_fact_evidence;
    }

    install_failure_evidence_from_download_facts(operation_id, facts)
}

pub(super) fn typed_runtime_failure_evidence(
    operation_id: &OperationId,
    error: &DownloadError,
) -> Option<ExecutionFact> {
    match error {
        DownloadError::RuntimeUnavailableForPlatform {
            component,
            platform,
        } => Some(install_execution_fact(
            operation_id,
            format!("java_runtime_{component}_{platform}"),
            OwnershipClass::LauncherManaged,
            ExecutionFactKind::RuntimeUnavailableForPlatform,
            [
                ("component", component.as_str()),
                ("platform", platform.as_str()),
            ],
        )),
        DownloadError::RuntimeRosettaRequired { component } => Some(install_execution_fact(
            operation_id,
            format!("java_runtime_{component}_rosetta"),
            OwnershipClass::LauncherManaged,
            ExecutionFactKind::RuntimeRosettaRequired,
            [("component", component.as_str())],
        )),
        DownloadError::RuntimeSource(failure) => {
            let component = failure.component().as_str();
            let kind = match failure.kind() {
                RuntimeSourceFailureKind::Unavailable => ExecutionFactKind::DownloadProviderFailure,
                RuntimeSourceFailureKind::MetadataInvalid
                | RuntimeSourceFailureKind::IntegrityMismatch
                | RuntimeSourceFailureKind::PolicyRejected => {
                    ExecutionFactKind::ProviderDataInvalid
                }
            };
            Some(install_execution_fact(
                operation_id,
                format!("java_runtime_source_{component}"),
                OwnershipClass::ExternalProviderDerived,
                kind,
                [
                    ("component", component),
                    ("source_failure_kind", failure.kind().as_str()),
                ],
            ))
        }
        DownloadError::PrepareRuntime(_) => Some(install_execution_fact(
            operation_id,
            "java_runtime",
            OwnershipClass::LauncherManaged,
            ExecutionFactKind::InstallExecutionFailed,
            [],
        )),
        _ => None,
    }
}

fn install_failure_evidence_from_download_error(
    operation_id: &OperationId,
    error: &DownloadError,
) -> Option<ExecutionFact> {
    let (target_id, kind) = install_failure_target_and_kind_from_download_error(error)?;

    Some(install_execution_fact(
        operation_id,
        target_id,
        OwnershipClass::LauncherManaged,
        kind,
        [],
    ))
}

pub(super) fn install_failure_target_and_kind_from_download_error(
    error: &DownloadError,
) -> Option<(&'static str, ExecutionFactKind)> {
    let evidence = match error {
        DownloadError::FileOperation(_) => {
            let kind = match error.file_failure_class()? {
                DownloadFileFailureClass::PermissionDenied => {
                    ExecutionFactKind::FilePermissionDenied
                }
                DownloadFileFailureClass::StorageFull => ExecutionFactKind::DownloadTempWriteFailed,
                DownloadFileFailureClass::NotFound => ExecutionFactKind::InstallDependencyFailed,
                DownloadFileFailureClass::Conflict | DownloadFileFailureClass::Unsettled => {
                    ExecutionFactKind::DownloadPromotionFailed
                }
                DownloadFileFailureClass::Interrupted | DownloadFileFailureClass::Other => {
                    ExecutionFactKind::InstallExecutionFailed
                }
            };
            ("install_filesystem", kind)
        }
        DownloadError::ResolveManifest(_) => (
            "version_manifest",
            ExecutionFactKind::DownloadProviderFailure,
        ),
        DownloadError::Request(_) => (
            "minecraft_download",
            ExecutionFactKind::DownloadNetworkFailure,
        ),
        DownloadError::ParseVersion(_) => ("version_json", ExecutionFactKind::ProviderDataInvalid),
        DownloadError::LibraryPlan(_) => {
            ("library_metadata", ExecutionFactKind::ProviderDataInvalid)
        }
        DownloadError::PrepareRuntime(_)
        | DownloadError::RuntimeSource(_)
        | DownloadError::RuntimeRosettaRequired { .. }
        | DownloadError::RuntimeUnavailableForPlatform { .. }
        | DownloadError::PublicationIndeterminate(_) => return None,
        DownloadError::Integrity(_) => return None,
    };

    Some(evidence)
}

fn execution_fact_from_download_fact(
    operation_id: &OperationId,
    fact: &ExecutionDownloadFact,
) -> ExecutionFact {
    let kind = match fact.kind {
        ExecutionDownloadFactKind::ChecksumMismatch => ExecutionFactKind::DownloadChecksumMismatch,
        ExecutionDownloadFactKind::MetadataInvalid | ExecutionDownloadFactKind::MetadataMissing => {
            ExecutionFactKind::ProviderDataInvalid
        }
        ExecutionDownloadFactKind::Interrupted => ExecutionFactKind::DownloadInterrupted,
        ExecutionDownloadFactKind::NetworkFailure => ExecutionFactKind::DownloadNetworkFailure,
        ExecutionDownloadFactKind::PermissionFailure => ExecutionFactKind::FilePermissionDenied,
        ExecutionDownloadFactKind::PromoteFailed => ExecutionFactKind::DownloadPromotionFailed,
        ExecutionDownloadFactKind::ProviderFailure => ExecutionFactKind::DownloadProviderFailure,
        ExecutionDownloadFactKind::SizeMismatch => ExecutionFactKind::DownloadSizeMismatch,
        ExecutionDownloadFactKind::TempDiscarded => ExecutionFactKind::DownloadTempDiscarded,
        ExecutionDownloadFactKind::TempWriteFailed => ExecutionFactKind::DownloadTempWriteFailed,
        ExecutionDownloadFactKind::WrittenToTemp => ExecutionFactKind::DownloadWrittenToTemp,
        ExecutionDownloadFactKind::Promoted => ExecutionFactKind::DownloadPromoted,
    };
    install_execution_fact(
        operation_id,
        &fact.target,
        OwnershipClass::LauncherManaged,
        kind,
        fact.fields
            .iter()
            .map(|(key, value)| (key.as_str(), value.as_str())),
    )
}

pub(super) fn install_execution_fact<'a>(
    operation_id: &OperationId,
    target_id: impl AsRef<str>,
    ownership: OwnershipClass,
    kind: ExecutionFactKind,
    fields: impl IntoIterator<Item = (&'a str, &'a str)>,
) -> ExecutionFact {
    let target_kind = match kind {
        ExecutionFactKind::RuntimeRosettaRequired
        | ExecutionFactKind::RuntimeUnavailableForPlatform => TargetKind::Runtime,
        ExecutionFactKind::InstallExecutionFailed | ExecutionFactKind::InstallProcessorFailed => {
            TargetKind::Version
        }
        _ => TargetKind::Artifact,
    };
    ExecutionFact {
        operation_id: Some(operation_id.clone()),
        kind,
        target: Some(TargetDescriptor::new(
            StabilizationSystem::Execution,
            target_kind,
            target_id.as_ref(),
            ownership,
        )),
        fields: fields
            .into_iter()
            .filter(|(key, _)| !install_field_key_looks_sensitive(key))
            .map(|(key, value)| EvidenceField::new(key, value, EvidenceSensitivity::Public))
            .collect(),
    }
}

fn install_field_key_looks_sensitive(key: &str) -> bool {
    let key = key.trim().to_ascii_lowercase();
    evidence_text_looks_sensitive(&key)
        || key.contains("user")
        || key.contains("account")
        || key.contains("uuid")
        || key.contains("token")
        || key.contains("secret")
        || key.contains("password")
        || key.contains("path")
        || key.contains("url")
        || key.contains("arg")
}

fn operation_evidence_batch(
    operation_id: &OperationId,
    phase: OperationPhase,
    facts: &[ExecutionFact],
) -> Result<OperationEvidenceBatch, OperationJournalStoreError> {
    OperationEvidenceBatch::try_from_execution_operation(operation_id, phase, facts)
        .map_err(|_| OperationJournalStoreError::InvalidGuardianOutcome)
}

fn terminal_download_failure_facts_for_error(
    error: &DownloadError,
    facts: &[ExecutionDownloadFact],
) -> Vec<ExecutionDownloadFact> {
    facts
        .iter()
        .filter(|fact| terminal_download_failure_fact_kind_for_error(error, fact.kind))
        .cloned()
        .collect()
}

fn should_prefer_terminal_download_facts(error: &DownloadError) -> bool {
    matches!(
        error,
        DownloadError::FileOperation(_) | DownloadError::Request(_) | DownloadError::Integrity(_)
    )
}

fn terminal_download_failure_fact_kind_for_error(
    error: &DownloadError,
    kind: ExecutionDownloadFactKind,
) -> bool {
    if matches!(error, DownloadError::Request(_)) {
        return request_terminal_download_failure_fact_kind(kind);
    }

    terminal_download_failure_fact_kind(kind)
}

fn request_terminal_download_failure_fact_kind(kind: ExecutionDownloadFactKind) -> bool {
    matches!(
        kind,
        ExecutionDownloadFactKind::Interrupted
            | ExecutionDownloadFactKind::NetworkFailure
            | ExecutionDownloadFactKind::ProviderFailure
    )
}

fn terminal_download_failure_fact_kind(kind: ExecutionDownloadFactKind) -> bool {
    matches!(
        kind,
        ExecutionDownloadFactKind::ChecksumMismatch
            | ExecutionDownloadFactKind::MetadataInvalid
            | ExecutionDownloadFactKind::MetadataMissing
            | ExecutionDownloadFactKind::Interrupted
            | ExecutionDownloadFactKind::NetworkFailure
            | ExecutionDownloadFactKind::PermissionFailure
            | ExecutionDownloadFactKind::PromoteFailed
            | ExecutionDownloadFactKind::ProviderFailure
            | ExecutionDownloadFactKind::SizeMismatch
            | ExecutionDownloadFactKind::TempWriteFailed
    )
}

async fn record_install_guardian_failure_outcome_without_memory(
    journals: &OperationJournalStore,
    operation_id: &OperationId,
    evidence: &[ExecutionFact],
    phase: OperationPhase,
) -> Result<(), OperationJournalStoreError> {
    let evidence_batch = operation_evidence_batch(operation_id, phase, evidence)?;
    let memory_window =
        ProviderFailureObservationWindow::from_observed_at(&journals.now_timestamp())
            .ok_or(OperationJournalStoreError::InvalidGuardianOutcome)?;
    settle_operation_guardian_failure(
        journals,
        None,
        operation_id,
        CommandKind::InstallVersion,
        &evidence_batch,
        phase,
        &memory_window,
        &operation_guardian_failure_terminal("install", &observed_install_failure_progress(), None),
    )
    .await
    .map_err(|error| match error {
        InstallGuardianSettlementError::Journal(error) => error,
        InstallGuardianSettlementError::Memory(_) => {
            OperationJournalStoreError::GuardianFailureMemoryUnavailable
        }
    })
}

pub(super) async fn record_install_guardian_failure_outcome(
    producer: &ProducerLease,
    journals: Arc<OperationJournalStore>,
    failure_memory: Arc<GuardianFailureMemoryStore>,
    operation_id: &OperationId,
    evidence: &[ExecutionFact],
    phase: OperationPhase,
) -> Result<(), OperationJournalStoreError> {
    record_operation_guardian_failure_outcome(
        producer,
        journals,
        failure_memory,
        OperationGuardianFailureRequest {
            operation_id,
            command: CommandKind::InstallVersion,
            evidence,
            phase,
            terminal: operation_guardian_failure_terminal(
                "install",
                &observed_install_failure_progress(),
                None,
            ),
        },
    )
    .await
}

struct OperationGuardianFailureRequest<'a> {
    operation_id: &'a OperationId,
    command: CommandKind,
    evidence: &'a [ExecutionFact],
    phase: OperationPhase,
    terminal: OperationGuardianFailureTerminal,
}

#[derive(Clone)]
struct OperationGuardianFailureTerminal {
    step: OperationJournalStep,
    failure_point: String,
}

fn operation_guardian_failure_terminal(
    step_namespace: &str,
    progress: &DownloadProgress,
    metrics: Option<&ContentDownloadMetrics>,
) -> OperationGuardianFailureTerminal {
    let phase = safe_progress_phase(&progress.phase);
    let mut step = install_progress_step(
        step_namespace,
        &phase,
        OperationStepResult::Failed,
        progress,
    );
    if let Some(metrics) = metrics {
        step.phase = OperationPhase::Downloading;
        step.set_metrics(OperationStepMetrics::ContentDownload(metrics.clone()));
    }
    OperationGuardianFailureTerminal {
        step,
        failure_point: format!("{step_namespace}_progress_{phase}"),
    }
}

fn assess_install_guardian_failure(
    failure_memory: Option<&GuardianFailureMemoryStore>,
    evidence: &OperationEvidenceBatch,
    phase: OperationPhase,
) -> Option<GuardianInstallAssessment> {
    let mode = GuardianMode::Managed;
    let context = failure_memory_suppression_context(failure_memory, mode, phase, evidence);
    assess_install_failure(mode, phase, evidence, context)
}

async fn record_operation_guardian_failure_outcome(
    producer: &ProducerLease,
    journals: Arc<OperationJournalStore>,
    failure_memory: Arc<GuardianFailureMemoryStore>,
    request: OperationGuardianFailureRequest<'_>,
) -> Result<(), OperationJournalStoreError> {
    let evidence = operation_evidence_batch(request.operation_id, request.phase, request.evidence)?;
    let operation_id = request.operation_id.clone();
    let logged_operation_id = operation_id.clone();
    let memory_window =
        ProviderFailureObservationWindow::from_observed_at(&failure_memory.now_timestamp())
            .ok_or(OperationJournalStoreError::InvalidGuardianOutcome)?;
    let command = request.command;
    let phase = request.phase;
    let terminal = request.terminal;
    #[cfg(test)]
    let policy_evaluation_count = crate::guardian::guardian_policy_evaluation_count_scope();
    let settlement = producer.claim_child().spawn_joinable(async move {
        let settlement = settle_owned_operation_guardian_failure(
            journals,
            failure_memory,
            operation_id,
            command,
            evidence,
            phase,
            memory_window,
            terminal,
        );
        #[cfg(test)]
        return crate::guardian::with_guardian_policy_evaluation_count_scope(
            policy_evaluation_count,
            settlement,
        )
        .await;
        #[cfg(not(test))]
        settlement.await
    });
    match settlement.await {
        Ok(result) => result,
        Err(error) => {
            let join_failure_kind = if error.is_panic() {
                "panic"
            } else if error.is_cancelled() {
                "cancelled"
            } else {
                "unknown"
            };
            warn!(
                operation_id = %logged_operation_id,
                join_failure_kind,
                "producer-owned Guardian install settlement stopped unexpectedly"
            );
            Err(OperationJournalStoreError::GuardianFailureMemoryUnavailable)
        }
    }
}

async fn settle_owned_operation_guardian_failure(
    journals: Arc<OperationJournalStore>,
    failure_memory: Arc<GuardianFailureMemoryStore>,
    operation_id: OperationId,
    command: CommandKind,
    evidence: OperationEvidenceBatch,
    phase: OperationPhase,
    memory_window: ProviderFailureObservationWindow,
    terminal: OperationGuardianFailureTerminal,
) -> Result<(), OperationJournalStoreError> {
    let _settlement = failure_memory.lock_install_guardian_settlement().await;
    let mut persistence_retries = 0;
    loop {
        match failure_memory.settle_install_guardian_pending().await {
            Ok(()) => {}
            Err(error)
                if retry_install_guardian_memory_persistence(
                    &error,
                    &mut persistence_retries,
                    "pending",
                )
                .await =>
            {
                continue;
            }
            Err(_) => return Err(OperationJournalStoreError::GuardianFailureMemoryUnavailable),
        }
        match settle_operation_guardian_failure(
            &journals,
            Some(&failure_memory),
            &operation_id,
            command,
            &evidence,
            phase,
            &memory_window,
            &terminal,
        )
        .await
        {
            Ok(()) => return Ok(()),
            Err(InstallGuardianSettlementError::Journal(error)) => return Err(error),
            Err(InstallGuardianSettlementError::Memory(error))
                if retry_install_guardian_memory_persistence(
                    &error,
                    &mut persistence_retries,
                    "publication",
                )
                .await => {}
            Err(InstallGuardianSettlementError::Memory(_)) => {
                return Err(OperationJournalStoreError::GuardianFailureMemoryUnavailable);
            }
        }
    }
}

async fn retry_install_guardian_memory_persistence(
    error: &FailureMemoryStoreError,
    retries: &mut usize,
    stage: &'static str,
) -> bool {
    let FailureMemoryStoreError::Persistence(error) = error else {
        return false;
    };
    if *retries >= INSTALL_GUARDIAN_MEMORY_RETRY_ATTEMPTS
        || !install_guardian_memory_persistence_is_retryable(error.kind())
    {
        return false;
    }
    *retries += 1;
    warn!(
        persistence_kind = ?error.kind(),
        retry_attempt = *retries,
        retry_limit = INSTALL_GUARDIAN_MEMORY_RETRY_ATTEMPTS,
        stage,
        "retrying Guardian install failure-memory persistence"
    );
    tokio::time::sleep(INSTALL_GUARDIAN_MEMORY_RETRY_DELAY).await;
    true
}

fn install_guardian_memory_persistence_is_retryable(kind: io::ErrorKind) -> bool {
    matches!(
        kind,
        io::ErrorKind::NotFound
            | io::ErrorKind::ConnectionRefused
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::HostUnreachable
            | io::ErrorKind::NetworkUnreachable
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::NotConnected
            | io::ErrorKind::NetworkDown
            | io::ErrorKind::BrokenPipe
            | io::ErrorKind::WouldBlock
            | io::ErrorKind::TimedOut
            | io::ErrorKind::WriteZero
            | io::ErrorKind::StaleNetworkFileHandle
            | io::ErrorKind::ResourceBusy
            | io::ErrorKind::ExecutableFileBusy
            | io::ErrorKind::Deadlock
            | io::ErrorKind::Interrupted
            | io::ErrorKind::UnexpectedEof
            | io::ErrorKind::Other
    )
}

enum InstallGuardianSettlementError {
    Journal(OperationJournalStoreError),
    Memory(FailureMemoryStoreError),
}

impl From<OperationJournalStoreError> for InstallGuardianSettlementError {
    fn from(error: OperationJournalStoreError) -> Self {
        Self::Journal(error)
    }
}

impl From<FailureMemoryStoreError> for InstallGuardianSettlementError {
    fn from(error: FailureMemoryStoreError) -> Self {
        Self::Memory(error)
    }
}

async fn settle_operation_guardian_failure(
    journals: &OperationJournalStore,
    failure_memory: Option<&GuardianFailureMemoryStore>,
    operation_id: &OperationId,
    command: CommandKind,
    evidence: &OperationEvidenceBatch,
    phase: OperationPhase,
    memory_window: &ProviderFailureObservationWindow,
    requested_terminal: &OperationGuardianFailureTerminal,
) -> Result<(), InstallGuardianSettlementError> {
    let existing_entry = journals.get(operation_id);
    if let Some(entry) = existing_entry.as_ref() {
        if !install_journal_identity_matches(entry, operation_id, command) {
            return Err(OperationJournalStoreError::InvalidGuardianOutcome.into());
        }
        if let Some(terminal) = entry.guardian_install_terminal() {
            if !entry
                .guardian_diagnosis_ids
                .contains(&terminal.diagnosis_id())
                || !operation_failure_terminal_is_visible(entry, requested_terminal)
            {
                return Err(OperationJournalStoreError::InvalidGuardianOutcome.into());
            }
            if !operation_evidence_matches_persisted_terminal(entry, evidence)
                && !persisted_provider_memory_is_already_settled(failure_memory, terminal)?
            {
                return Err(OperationJournalStoreError::InvalidGuardianOutcome.into());
            }
            let memory = terminal
                .memory()
                .map(GuardianInstallOutcomeMemoryPersistence::from_durable);
            publish_provider_failure_memory_if_needed(
                failure_memory,
                ProviderFailureMemoryPublicationRequest {
                    mode: GuardianMode::Managed,
                    diagnosis_id: terminal.diagnosis_id(),
                    retry: terminal.action() == GuardianActionKind::Retry,
                    publication: ProviderMemoryPublication::Replay(memory),
                },
            )
            .await?;
            return Ok(());
        }
    }

    let Some(assessment) = assess_install_guardian_failure(failure_memory, evidence, phase) else {
        if existing_entry
            .as_ref()
            .is_some_and(|entry| operation_failure_terminal_is_visible(entry, requested_terminal))
        {
            return Ok(());
        }
        record_operation_failure_terminal_with_reconciliation(
            journals,
            operation_id,
            command,
            requested_terminal,
            None,
        )
        .await?;
        return Ok(());
    };
    let outcome = assessment.terminal_outcome();
    let Some((durable, memory)) = assessment.durable_terminal_evidence(
        evidence,
        GuardianMode::Managed,
        PROVIDER_FAILURE_MEMORY_SOURCE,
        &memory_window.observed_at,
        &memory_window.suppression_until,
    ) else {
        record_operation_failure_terminal_with_reconciliation(
            journals,
            operation_id,
            command,
            requested_terminal,
            None,
        )
        .await?;
        return Ok(());
    };
    if let Some(entry) = existing_entry.as_ref().filter(|entry| {
        entry.status == OperationStatus::Failed && entry.outcome == Some(OperationOutcome::Failed)
    }) {
        if !operation_failure_terminal_is_visible(entry, requested_terminal) {
            return Err(OperationJournalStoreError::InvalidGuardianOutcome.into());
        }
        record_guardian_evidence_with_reconciliation(journals, command, &durable).await?;
    } else {
        match record_operation_failure_terminal_with_reconciliation(
            journals,
            operation_id,
            command,
            requested_terminal,
            Some(&durable),
        )
        .await
        {
            Ok(()) => {}
            Err(OperationJournalStoreError::AlreadyTerminal) => {
                let entry = journals
                    .get(operation_id)
                    .filter(|entry| {
                        entry.status == OperationStatus::Failed
                            && entry.outcome == Some(OperationOutcome::Failed)
                            && operation_failure_terminal_is_visible(entry, requested_terminal)
                    })
                    .ok_or(OperationJournalStoreError::InvalidGuardianOutcome)?;
                if entry.guardian_install_terminal().is_some() {
                    return Err(OperationJournalStoreError::InvalidGuardianOutcome.into());
                }
                record_guardian_evidence_with_reconciliation(journals, command, &durable).await?;
            }
            Err(error) => return Err(error.into()),
        }
    }
    if let (Some(outcome), Some(memory)) = (outcome, memory) {
        publish_provider_failure_memory_if_needed(
            failure_memory,
            ProviderFailureMemoryPublicationRequest {
                mode: GuardianMode::Managed,
                diagnosis_id: outcome.diagnosis_id,
                retry: true,
                publication: ProviderMemoryPublication::Assessed(memory),
            },
        )
        .await?;
    }
    Ok(())
}

fn operation_evidence_matches_persisted_terminal(
    entry: &OperationJournalEntry,
    evidence: &OperationEvidenceBatch,
) -> bool {
    let Some(step) = entry.completed_steps.last() else {
        return false;
    };
    if evidence
        .facts()
        .iter()
        .any(|fact| !step.guardian_fact_ids().contains(&fact.id))
    {
        return false;
    }
    let Some(memory) = entry
        .guardian_install_terminal()
        .and_then(|terminal| terminal.memory())
    else {
        return true;
    };
    evidence
        .facts()
        .iter()
        .filter_map(|fact| fact.target.as_ref())
        .any(|target| target == memory.target())
}

fn persisted_provider_memory_is_already_settled(
    failure_memory: Option<&GuardianFailureMemoryStore>,
    terminal: &crate::state::contracts::GuardianInstallTerminalEvidence,
) -> Result<bool, FailureMemoryStoreError> {
    if terminal.action() != GuardianActionKind::Retry {
        return Ok(true);
    }
    let Some(store) = failure_memory else {
        return Ok(true);
    };
    let Some(memory) = terminal.memory() else {
        return Ok(false);
    };
    let memory = GuardianInstallOutcomeMemoryPersistence::from_durable(memory);
    let expected = GuardianFailureMemoryEntry::observed(
        terminal.diagnosis_id(),
        GuardianDomain::Download,
        memory.target().clone(),
        GuardianMode::Managed,
        Some(PROVIDER_FAILURE_MEMORY_SOURCE),
        memory.observed_at().to_string(),
    )
    .with_action(
        GuardianActionKind::Retry,
        FailureMemoryActionOutcome::Retried,
    )
    .with_suppression_until(memory.suppression_until().to_string());
    if !memory.matches_failure_memory_key(&expected.key, &expected.target) {
        return Ok(false);
    }
    let expected = match store.construct_entry(expected) {
        Ok(expected) => expected,
        Err(FailureMemoryStoreError::Expired) => return Ok(true),
        Err(error) => return Err(error),
    };
    Ok(store.get(&expected.key).as_ref() == Some(&expected))
}

fn operation_failure_terminal_is_visible(
    entry: &OperationJournalEntry,
    terminal: &OperationGuardianFailureTerminal,
) -> bool {
    install_progress_transition_matches(
        entry,
        &entry.operation_id,
        entry.command,
        &terminal.step,
        true,
        Some(&terminal.failure_point),
    )
}

async fn record_operation_failure_terminal_with_reconciliation(
    journals: &OperationJournalStore,
    operation_id: &OperationId,
    command: CommandKind,
    terminal: &OperationGuardianFailureTerminal,
    evidence: Option<&DurableGuardianEvidence>,
) -> Result<(), OperationJournalStoreError> {
    loop {
        let result = if let Some(evidence) = evidence {
            journals
                .record_failure_with_guardian_evidence(
                    terminal.step.clone(),
                    terminal.failure_point.clone(),
                    OperationOutcome::Failed,
                    evidence.clone(),
                )
                .await
        } else if terminal.step.metrics().is_some() {
            journals
                .record_failure_with_metrics(
                    operation_id,
                    terminal.step.clone(),
                    terminal.failure_point.clone(),
                )
                .await
        } else {
            journals
                .record_failure(
                    operation_id,
                    terminal.step.clone(),
                    terminal.failure_point.clone(),
                    OperationOutcome::Failed,
                )
                .await
        };
        match result {
            Ok(()) => return Ok(()),
            Err(error) => {
                match reconcile_install_journal_error(journals, operation_id, error, |entry| {
                    if let Some(evidence) = evidence {
                        install_failure_with_evidence_matches(
                            entry,
                            operation_id,
                            command,
                            &terminal.step,
                            &terminal.failure_point,
                            evidence,
                        )
                    } else {
                        install_progress_transition_matches(
                            entry,
                            operation_id,
                            command,
                            &terminal.step,
                            true,
                            Some(&terminal.failure_point),
                        )
                    }
                })
                .await?
                {
                    InstallJournalReconciliation::MutationCommitted => return Ok(()),
                    InstallJournalReconciliation::RetryMutation => {}
                }
            }
        }
    }
}

async fn record_guardian_evidence_with_reconciliation(
    journals: &OperationJournalStore,
    command: CommandKind,
    evidence: &DurableGuardianEvidence,
) -> Result<(), OperationJournalStoreError> {
    let operation_id = evidence.operation_id();
    loop {
        if journals
            .get(operation_id)
            .as_ref()
            .is_some_and(|entry| install_guardian_evidence_is_visible(entry, command, evidence))
        {
            return Ok(());
        }
        match journals.record_guardian_evidence(evidence.clone()).await {
            Ok(()) => return Ok(()),
            Err(error) => {
                match reconcile_install_journal_error(journals, operation_id, error, |entry| {
                    install_guardian_evidence_is_visible(entry, command, evidence)
                })
                .await?
                {
                    InstallJournalReconciliation::MutationCommitted => return Ok(()),
                    InstallJournalReconciliation::RetryMutation => {}
                }
            }
        }
    }
}

fn install_guardian_evidence_is_visible(
    entry: &OperationJournalEntry,
    command: CommandKind,
    evidence: &DurableGuardianEvidence,
) -> bool {
    install_journal_identity_matches(entry, evidence.operation_id(), command)
        && entry.completed_steps.last().is_some_and(|step| {
            evidence
                .fact_ids()
                .iter()
                .all(|fact_id| step.guardian_fact_ids().contains(fact_id))
        })
        && evidence
            .diagnosis_ids()
            .iter()
            .all(|diagnosis_id| entry.guardian_diagnosis_ids.contains(diagnosis_id))
        && evidence
            .install_terminal()
            .is_none_or(|terminal| entry.guardian_install_terminal() == Some(terminal))
}

fn install_guardian_terminal_update(
    assessment: &GuardianInstallAssessment,
    evidence: &OperationEvidenceBatch,
    memory_window: &ProviderFailureObservationWindow,
) -> Option<DurableGuardianEvidence> {
    assessment.terminal_outcome()?;
    assessment
        .durable_terminal_evidence(
            evidence,
            GuardianMode::Managed,
            PROVIDER_FAILURE_MEMORY_SOURCE,
            &memory_window.observed_at,
            &memory_window.suppression_until,
        )
        .map(|(durable, _)| durable)
}

fn install_journal_identity_matches(
    entry: &OperationJournalEntry,
    operation_id: &OperationId,
    command: CommandKind,
) -> bool {
    &entry.operation_id == operation_id
        && entry.command == command
        && entry.owner == StabilizationSystem::Application
        && entry.ownership == OwnershipClass::LauncherManaged
}

fn install_progress_transition_matches(
    entry: &OperationJournalEntry,
    operation_id: &OperationId,
    command: CommandKind,
    step: &OperationJournalStep,
    terminal: bool,
    failure_point: Option<&str>,
) -> bool {
    if !install_journal_identity_matches(entry, operation_id, command)
        || !operation_journal_completed_step_is_visible(entry, step)
    {
        return false;
    }
    if !terminal {
        return entry.status == OperationStatus::Running
            && entry.outcome.is_none()
            && entry.failure_point.is_none();
    }
    if let Some(failure_point) = failure_point {
        entry.status == OperationStatus::Failed
            && entry.outcome == Some(OperationOutcome::Failed)
            && entry.failure_point.as_deref() == Some(failure_point)
    } else {
        entry.status == OperationStatus::Succeeded
            && entry.outcome == Some(OperationOutcome::Succeeded)
            && entry.failure_point.is_none()
    }
}

fn install_failure_with_evidence_matches(
    entry: &OperationJournalEntry,
    operation_id: &OperationId,
    command: CommandKind,
    step: &OperationJournalStep,
    failure_point: &str,
    evidence: &DurableGuardianEvidence,
) -> bool {
    install_journal_identity_matches(entry, operation_id, command)
        && entry.status == OperationStatus::Failed
        && entry.outcome == Some(OperationOutcome::Failed)
        && entry.failure_point.as_deref() == Some(failure_point)
        && operation_journal_completed_step_is_visible(entry, step)
        && entry.completed_steps.last().is_some_and(|completed| {
            evidence
                .fact_ids()
                .iter()
                .all(|fact_id| completed.guardian_fact_ids().contains(fact_id))
        })
        && evidence
            .diagnosis_ids()
            .iter()
            .all(|diagnosis_id| entry.guardian_diagnosis_ids.contains(diagnosis_id))
        && entry.guardian_install_terminal() == evidence.install_terminal()
}

pub(crate) const fn loader_install_guardian_evidence_kind(
    failure_kind: LoaderInstallFailureKind,
) -> (ExecutionFactKind, OwnershipClass, OperationPhase) {
    match failure_kind {
        LoaderInstallFailureKind::ProviderHttpFailure
        | LoaderInstallFailureKind::ProviderRateLimited
        | LoaderInstallFailureKind::ArtifactMissing => (
            ExecutionFactKind::DownloadProviderFailure,
            OwnershipClass::ExternalProviderDerived,
            OperationPhase::Downloading,
        ),
        LoaderInstallFailureKind::ProviderNetworkFailure => (
            ExecutionFactKind::DownloadNetworkFailure,
            OwnershipClass::ExternalProviderDerived,
            OperationPhase::Downloading,
        ),
        LoaderInstallFailureKind::ProviderResponseTooLarge
        | LoaderInstallFailureKind::ProviderSchemaInvalid
        | LoaderInstallFailureKind::InvalidProfile => (
            ExecutionFactKind::ProviderDataInvalid,
            OwnershipClass::ExternalProviderDerived,
            OperationPhase::Downloading,
        ),
        LoaderInstallFailureKind::ParseFailed
        | LoaderInstallFailureKind::VerifyFailed
        | LoaderInstallFailureKind::InstallExecutionFailed => (
            ExecutionFactKind::InstallExecutionFailed,
            OwnershipClass::LauncherManaged,
            OperationPhase::Installing,
        ),
        LoaderInstallFailureKind::ProcessorFailed => (
            ExecutionFactKind::InstallProcessorFailed,
            OwnershipClass::LauncherManaged,
            OperationPhase::Installing,
        ),
    }
}

fn loader_error_guardian_failure_evidence(
    operation_id: &OperationId,
    target_id: &str,
    failure: &LoaderActiveInstallFailure,
    failure_kind: LoaderInstallFailureKind,
    kind: ExecutionFactKind,
    ownership: OwnershipClass,
) -> ExecutionFact {
    let mut fields = vec![("failure_kind", failure_kind.as_str().to_string())];
    if let Some(provider_kind) = failure.source().provider_failure_kind() {
        fields.push(("provider_failure", provider_kind.as_str().to_string()));
    }
    if let Some(status) = failure.source().provider_status() {
        fields.push(("status", status.to_string()));
    }
    install_execution_fact(
        operation_id,
        target_id,
        ownership,
        kind,
        fields.iter().map(|(key, value)| (*key, value.as_str())),
    )
}

fn failure_memory_suppression_context(
    failure_memory: Option<&GuardianFailureMemoryStore>,
    mode: GuardianMode,
    phase: OperationPhase,
    evidence: &OperationEvidenceBatch,
) -> GuardianPolicyContext {
    let mut context = GuardianPolicyContext::current_operation();
    if provider_failure_memory_entry(failure_memory, mode, phase, evidence).is_some() {
        context = context.with_suppression();
    }
    context
}

fn provider_failure_memory_entry(
    failure_memory: Option<&GuardianFailureMemoryStore>,
    mode: GuardianMode,
    phase: OperationPhase,
    evidence: &OperationEvidenceBatch,
) -> Option<crate::state::failure_memory::GuardianFailureMemoryEntry> {
    let memory = failure_memory?;
    let key = install_failure_memory_key(mode, phase, evidence, DiagnosisId::DownloadUnavailable)?;
    let entry = memory.get(&key)?;
    if !memory.suppression_active(&entry) {
        return None;
    }
    Some(entry)
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum ProviderMemoryPublication {
    Assessed(GuardianInstallOutcomeMemoryPersistence),
    Replay(Option<GuardianInstallOutcomeMemoryPersistence>),
}

struct ProviderFailureMemoryPublicationRequest {
    mode: GuardianMode,
    diagnosis_id: DiagnosisId,
    retry: bool,
    publication: ProviderMemoryPublication,
}

async fn publish_provider_failure_memory_if_needed(
    failure_memory: Option<&GuardianFailureMemoryStore>,
    request: ProviderFailureMemoryPublicationRequest,
) -> Result<(), FailureMemoryStoreError> {
    let ProviderFailureMemoryPublicationRequest {
        mode,
        diagnosis_id,
        retry,
        publication,
    } = request;
    if diagnosis_id != DiagnosisId::DownloadUnavailable || !retry {
        return Ok(());
    }
    let Some(memory) = failure_memory else {
        return Ok(());
    };
    let replay = matches!(&publication, ProviderMemoryPublication::Replay(_));
    let memory_persistence = match publication {
        ProviderMemoryPublication::Assessed(memory) => memory,
        ProviderMemoryPublication::Replay(Some(memory)) => memory,
        ProviderMemoryPublication::Replay(None) => return Ok(()),
    };
    let target = memory_persistence.target().clone();
    let entry = GuardianFailureMemoryEntry::observed(
        diagnosis_id,
        GuardianDomain::Download,
        target,
        mode,
        Some(PROVIDER_FAILURE_MEMORY_SOURCE),
        memory_persistence.observed_at().to_string(),
    )
    .with_action(
        GuardianActionKind::Retry,
        FailureMemoryActionOutcome::Retried,
    )
    .with_suppression_until(memory_persistence.suppression_until().to_string());
    if !memory_persistence.matches_failure_memory_key(&entry.key, &entry.target) {
        return Ok(());
    }
    let entry = match memory.construct_entry(entry) {
        Ok(entry) => entry,
        Err(FailureMemoryStoreError::Expired) if replay => return Ok(()),
        Err(error) => return Err(error),
    };
    if replay {
        return memory.reconcile_install_guardian_retry(entry).await;
    }
    memory.record_install_guardian_retry(entry).await
}

fn install_failure_memory_key(
    mode: GuardianMode,
    phase: OperationPhase,
    evidence: &OperationEvidenceBatch,
    diagnosis_id: DiagnosisId,
) -> Option<FailureMemoryKey> {
    let safety_case = crate::guardian::build_safety_case(mode, phase, evidence);
    let diagnosis = safety_case
        .diagnoses
        .iter()
        .find(|diagnosis| diagnosis.id() == diagnosis_id)?;
    let target = diagnosis.affected_targets().first()?;
    Some(FailureMemoryKey::for_observation(
        diagnosis.domain(),
        &diagnosis.id(),
        target,
        mode,
        Some(PROVIDER_FAILURE_MEMORY_SOURCE),
    ))
}

pub(super) fn public_install_id(id: &str) -> String {
    sanitize_evidence_token(id, RedactionAudience::UserVisible, 96)
        .unwrap_or_else(|| "install".to_string())
}

pub(crate) fn interrupted_install_progress() -> DownloadProgress {
    observed_install_failure_progress()
}

pub(crate) fn publication_indeterminate_install_progress() -> DownloadProgress {
    DownloadProgress {
        phase: "recovering".to_string(),
        current: 0,
        total: 0,
        file: None,
        error: None,
        done: false,
        bytes_done: None,
        bytes_total: None,
    }
}

pub(crate) fn observed_install_failure_progress() -> DownloadProgress {
    DownloadProgress {
        phase: "error".to_string(),
        current: 0,
        total: 0,
        file: None,
        error: Some(INSTALL_FAILURE_MESSAGE.to_string()),
        done: true,
        bytes_done: None,
        bytes_total: None,
    }
}

pub(super) fn install_journal_is_terminal(status: OperationStatus) -> bool {
    matches!(
        status,
        OperationStatus::Succeeded
            | OperationStatus::Failed
            | OperationStatus::Blocked
            | OperationStatus::Cancelled
    )
}

pub(super) fn install_failure_point_from_journal(entry: &OperationJournalEntry) -> Option<String> {
    entry.failure_point.as_deref().and_then(|failure_point| {
        sanitize_evidence_token(failure_point, RedactionAudience::UserVisible, 96)
    })
}

pub(super) fn install_progress_history_from_journal(
    entry: &OperationJournalEntry,
) -> Vec<DownloadProgress> {
    let mut history = entry
        .completed_steps
        .iter()
        .filter_map(progress_from_install_journal_step)
        .collect::<Vec<_>>();

    if install_journal_is_terminal(entry.status) && !history.iter().any(|progress| progress.done) {
        history.push(terminal_progress_for_journal_status(entry.status));
    }

    history
}

pub(crate) fn authoritative_install_terminal_progress(
    journals: &OperationJournalStore,
    operation_id: &OperationId,
) -> Option<DownloadProgress> {
    let entry = journals.get(operation_id)?;
    if !install_journal_is_terminal(entry.status) {
        return None;
    }
    install_progress_history_from_journal(&entry)
        .into_iter()
        .rev()
        .find(|progress| progress.done)
        .map(sanitize_install_progress)
}

fn progress_from_install_journal_step(step: &OperationJournalStep) -> Option<DownloadProgress> {
    let phase = install_phase_fact_value(step)?;
    let done = step
        .generated_facts
        .iter()
        .any(|fact| fact == "install_done:true");
    let failed = step
        .generated_facts
        .iter()
        .any(|fact| fact == "install_error:true")
        || step.result == OperationStepResult::Failed;

    Some(DownloadProgress {
        phase,
        current: if done && !failed { 1 } else { 0 },
        total: if done && !failed { 1 } else { 0 },
        file: None,
        error: (done && failed).then(|| INSTALL_FAILURE_MESSAGE.to_string()),
        done,
        bytes_done: None,
        bytes_total: None,
    })
}

fn install_phase_fact_value(step: &OperationJournalStep) -> Option<String> {
    step.generated_facts.iter().find_map(|fact| {
        fact.strip_prefix("install_phase:")
            .and_then(|phase| sanitize_evidence_token(phase, RedactionAudience::UserVisible, 48))
    })
}

fn terminal_progress_for_journal_status(status: OperationStatus) -> DownloadProgress {
    if status == OperationStatus::Succeeded {
        return DownloadProgress {
            phase: "done".to_string(),
            current: 1,
            total: 1,
            file: None,
            error: None,
            done: true,
            bytes_done: None,
            bytes_total: None,
        };
    }

    observed_install_failure_progress()
}

fn install_progress_step(
    step_namespace: &str,
    phase: &str,
    result: OperationStepResult,
    progress: &DownloadProgress,
) -> OperationJournalStep {
    let mut step = install_journal_step(
        format!("{step_namespace}_progress_{phase}"),
        install_operation_phase(progress),
        result,
        None,
    );
    step.generated_facts.push(format!("install_phase:{phase}"));
    if progress.done {
        step.generated_facts.push("install_done:true".to_string());
    }
    if progress.error.is_some() {
        step.generated_facts.push("install_error:true".to_string());
    }
    step
}

fn install_journal_step(
    step_id: impl AsRef<str>,
    phase: OperationPhase,
    result: OperationStepResult,
    changed_target: Option<TargetDescriptor>,
) -> OperationJournalStep {
    let step_id = sanitize_evidence_token(step_id.as_ref(), RedactionAudience::UserVisible, 96)
        .unwrap_or_else(|| "install_step".to_string());
    let mut step = OperationJournalStep::new(step_id, phase);
    step.result = result;
    step.changed_target = changed_target;
    step.rollback = RollbackState::NotApplicable;
    step
}

fn install_operation_phase(progress: &DownloadProgress) -> OperationPhase {
    if progress.done && progress.error.is_some() {
        return OperationPhase::Failed;
    }
    if progress.done {
        return OperationPhase::Completed;
    }

    match progress.phase.trim() {
        "version_json" | "client_jar" | "libraries" | "asset_index" | "assets" | "log_config"
        | "java_runtime" | "java_runtime_ready" | "artifacts" | "loader_libraries" | "download" => {
            OperationPhase::Downloading
        }
        "profile" | "processors" | "loader_overlay" | "loader_publish" => {
            OperationPhase::Installing
        }
        "planning" => OperationPhase::Planning,
        "overrides" | "commit" | "removing" => OperationPhase::Installing,
        "recovering" => OperationPhase::Repairing,
        _ => OperationPhase::Running,
    }
}

fn install_version_target(version_id: &str) -> TargetDescriptor {
    TargetDescriptor::new(
        StabilizationSystem::Application,
        TargetKind::Version,
        version_id,
        OwnershipClass::LauncherManaged,
    )
}

fn install_session_target(install_id: &str) -> TargetDescriptor {
    TargetDescriptor::new(
        StabilizationSystem::Application,
        TargetKind::Session,
        install_id,
        OwnershipClass::LauncherManaged,
    )
}

fn content_instance_target(instance_id: &str) -> TargetDescriptor {
    TargetDescriptor::new(
        StabilizationSystem::Application,
        TargetKind::Instance,
        instance_id,
        OwnershipClass::LauncherManaged,
    )
}

fn safe_progress_phase(phase: &str) -> String {
    sanitize_evidence_token(phase, RedactionAudience::UserVisible, 48)
        .unwrap_or_else(|| "install".to_string())
}

#[cfg(test)]
mod operation_id_tests {
    use super::*;
    use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
    use sha2::{Digest as _, Sha256};

    fn publication_evidence(version_id: &str) -> ManagedInstallPublicationEvidenceId {
        ManagedInstallPublicationEvidenceId::parse(&format!(
            "managed-install-v1.{}.{}.{}.{}.{}",
            URL_SAFE_NO_PAD.encode(Sha256::digest(version_id.as_bytes())),
            URL_SAFE_NO_PAD.encode([1_u8; 16]),
            URL_SAFE_NO_PAD.encode([2_u8; 16]),
            URL_SAFE_NO_PAD.encode([3_u8; 32]),
            URL_SAFE_NO_PAD.encode([4_u8; 32]),
        ))
        .expect("canonical publication evidence")
    }

    #[tokio::test]
    async fn install_session_lookup_uses_journal_sequence() {
        let journals = OperationJournalStore::new();
        let first = OperationId::try_from("op-ffffffff-ffff-4fff-8fff-ffffffffffff")
            .expect("valid first operation id");
        let second = OperationId::try_from("op-00000000-0000-4000-8000-000000000000")
            .expect("valid second operation id");

        journals
            .create(planned_install_journal_for_session(
                &first,
                "retained-session",
                &InstallJournalIdentity::vanilla("1.20.4"),
            ))
            .await
            .expect("create first session journal");
        journals
            .create(planned_install_journal_for_session(
                &second,
                "retained-session",
                &InstallJournalIdentity::vanilla("1.20.4"),
            ))
            .await
            .expect("create second session journal");

        assert_eq!(
            install_operation_journal_for_session(&journals, "retained-session")
                .expect("latest retained session journal")
                .operation_id,
            second
        );
    }

    #[tokio::test]
    async fn fresh_install_journal_refuses_matching_identity_collision() {
        let journals = OperationJournalStore::new();
        let operation_id = OperationId::try_from("op-ffffffff-ffff-4fff-8fff-ffffffffffff")
            .expect("valid collision id");
        let expected = planned_install_journal_for_session(
            &operation_id,
            "colliding-session",
            &InstallJournalIdentity::vanilla("1.20.4"),
        );
        journals
            .create(expected)
            .await
            .expect("seed matching collision");

        assert!(matches!(
            begin_install_operation_journal_for_session(
                &journals,
                &operation_id,
                "colliding-session",
                &InstallJournalIdentity::vanilla("1.20.4"),
            )
            .await,
            Err(OperationJournalStoreError::AlreadyExists)
        ));
    }

    #[tokio::test]
    async fn startup_scan_refuses_distinct_install_identities_on_one_root_lane() {
        let journals = OperationJournalStore::new();
        for (operation_id, install_id, version_id) in [
            (
                "op-11111111-1111-4111-8111-111111111111",
                "install-11111111111111111111111111111111",
                "1.20.4",
            ),
            (
                "op-22222222-2222-4222-8222-222222222222",
                "install-22222222222222222222222222222222",
                "1.21.5",
            ),
        ] {
            let operation_id = OperationId::try_from(operation_id).expect("valid operation id");
            journals
                .create(planned_install_journal_for_session(
                    &operation_id,
                    install_id,
                    &InstallJournalIdentity::vanilla(version_id),
                ))
                .await
                .expect("create nonterminal install journal");
        }

        assert_eq!(
            recovering_install_journals(&journals),
            Err(RecoveringInstallJournalError::DuplicateRootLane)
        );
    }

    #[test]
    fn publication_checkpoint_requires_canonical_evidence_for_exact_version() {
        let version_id = "1.21.5";
        let identity = InstallJournalIdentity::vanilla(version_id);
        let activation_contract =
            "managed-install-activation-v1.qqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqo";
        let checkpoint_step = |evidence: &str| {
            let mut step = install_journal_step(
                INSTALL_PUBLICATION_COMMITTED_STEP,
                OperationPhase::Installing,
                OperationStepResult::Completed,
                Some(install_version_target(version_id)),
            );
            step.generated_facts = vec![
                format!(
                    "{INSTALL_PUBLICATION_FACT_PREFIX}{}",
                    InstallPublicationCheckpointKind::Committed.fact()
                ),
                format!("{INSTALL_PUBLICATION_VERSION_ID_FACT_PREFIX}{version_id}"),
                format!("{INSTALL_PUBLICATION_EVIDENCE_FACT_PREFIX}{evidence}"),
                format!("{INSTALL_ACTIVATION_CONTRACT_FACT_PREFIX}{activation_contract}"),
            ];
            step
        };
        let valid = publication_evidence(version_id);
        assert!(
            parse_install_publication_checkpoint(
                &checkpoint_step(valid.as_str()),
                InstallPublicationCheckpointKind::Committed,
                &identity,
            )
            .is_ok()
        );

        let wrong_version = publication_evidence("1.20.4");
        let invalid_nonce = {
            let mut segments = valid.as_str().split('.').collect::<Vec<_>>();
            segments[2] = "**********************";
            segments.join(".")
        };
        let short_fingerprint = valid.as_str()[..valid.as_str().len() - 1].to_string();
        for malformed in [
            wrong_version.as_str(),
            invalid_nonce.as_str(),
            short_fingerprint.as_str(),
        ] {
            assert_eq!(
                parse_install_publication_checkpoint(
                    &checkpoint_step(malformed),
                    InstallPublicationCheckpointKind::Committed,
                    &identity,
                ),
                Err(RecoveringInstallJournalError::Malformed)
            );
        }
    }

    #[test]
    fn committed_checkpoint_facts_bind_contract_after_publication_evidence() {
        let contract = axial_minecraft::ManagedInstallActivationContractId::parse(
            "managed-install-activation-v1.qqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqo",
        )
        .expect("canonical test activation contract");
        for (kind, version_id) in [
            (InstallPublicationCheckpointKind::Committed, "1.21.5"),
            (InstallPublicationCheckpointKind::BaseCommitted, "1.21.5"),
            (
                InstallPublicationCheckpointKind::ChildCommitted,
                "fabric-loader-0.16.14-1.21.5",
            ),
        ] {
            let evidence = publication_evidence(version_id);
            let checkpoint = InstallPublicationCheckpoint {
                kind,
                version_id: version_id.to_string(),
                evidence: evidence.clone(),
                activation_contract_id: Some(contract.clone()),
            };
            let step =
                install_publication_checkpoint_step(&checkpoint).expect("committed checkpoint");
            assert_eq!(
                step.generated_facts,
                vec![
                    format!("{INSTALL_PUBLICATION_FACT_PREFIX}{}", kind.fact()),
                    format!("{INSTALL_PUBLICATION_VERSION_ID_FACT_PREFIX}{version_id}"),
                    format!("{INSTALL_PUBLICATION_EVIDENCE_FACT_PREFIX}{evidence}"),
                    format!("{INSTALL_ACTIVATION_CONTRACT_FACT_PREFIX}{contract}"),
                ]
            );
        }

        let version_id = "1.21.5";
        let evidence = publication_evidence(version_id);
        let rollback = install_publication_checkpoint_step(&InstallPublicationCheckpoint {
            kind: InstallPublicationCheckpointKind::RolledBack,
            version_id: version_id.to_string(),
            evidence: evidence.clone(),
            activation_contract_id: None,
        })
        .expect("rollback checkpoint");
        assert_eq!(
            rollback.generated_facts,
            vec![
                format!(
                    "{INSTALL_PUBLICATION_FACT_PREFIX}{}",
                    InstallPublicationCheckpointKind::RolledBack.fact()
                ),
                format!("{INSTALL_PUBLICATION_VERSION_ID_FACT_PREFIX}{version_id}"),
                format!("{INSTALL_PUBLICATION_EVIDENCE_FACT_PREFIX}{evidence}"),
            ]
        );
    }

    #[tokio::test]
    async fn loader_child_checkpoint_facts_cross_strict_journal_persistence() {
        let component_id = LoaderComponentId::Fabric;
        let base_version_id = "1.21.5";
        let loader_version = "0.16.14";
        let target_version_id =
            installed_version_id_for(component_id, base_version_id, loader_version)
                .expect("canonical loader target");
        let identity = InstallJournalIdentity::Loader {
            target_version_id: target_version_id.clone(),
            component_id,
            build_id: axial_minecraft::build_id_for(component_id, base_version_id, loader_version),
            base_version_id: base_version_id.to_string(),
        };
        let contract = axial_minecraft::ManagedInstallActivationContractId::parse(
            "managed-install-activation-v1.qqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqo",
        )
        .expect("canonical test activation contract");

        for (install_id, suffix, kind, activation_contract_id) in [
            (
                "loader-install-11111111111111111111111111111111",
                "committed",
                InstallPublicationCheckpointKind::ChildCommitted,
                Some(contract.clone()),
            ),
            (
                "loader-install-22222222222222222222222222222222",
                "rolled-back",
                InstallPublicationCheckpointKind::RolledBack,
                None,
            ),
        ] {
            let journals = OperationJournalStore::new();
            let operation_id = OperationId::deterministic_test(format!("loader-child-{suffix}"));
            begin_install_operation_journal_for_session(
                &journals,
                &operation_id,
                install_id,
                &identity,
            )
            .await
            .expect("persist loader install plan");
            let mut recovering = install_journal_step(
                INSTALL_RECOVERING_STEP,
                OperationPhase::Repairing,
                OperationStepResult::Completed,
                None,
            );
            recovering.generated_facts = vec!["install_phase:recovering".to_string()];
            journals
                .record_checkpoint(&operation_id, recovering)
                .await
                .expect("persist loader recovery marker");
            let base_checkpoint = InstallPublicationCheckpoint {
                kind: InstallPublicationCheckpointKind::BaseCommitted,
                version_id: base_version_id.to_string(),
                evidence: publication_evidence(base_version_id),
                activation_contract_id: Some(contract.clone()),
            };
            record_install_publication_checkpoint(&journals, &operation_id, &base_checkpoint)
                .await
                .expect("persist loader base checkpoint");
            let checkpoint = InstallPublicationCheckpoint {
                kind,
                version_id: target_version_id.clone(),
                evidence: publication_evidence(&target_version_id),
                activation_contract_id,
            };

            record_install_publication_checkpoint(&journals, &operation_id, &checkpoint)
                .await
                .expect("persist opaque loader child checkpoint");

            let recovered = recovering_install_journal(&journals, &operation_id)
                .expect("strictly parse persisted loader child checkpoint");
            assert_eq!(recovered.checkpoints, vec![base_checkpoint, checkpoint]);
        }
    }

    #[test]
    fn vanilla_journal_identity_rejects_noncanonical_version_ids() {
        for version_id in ["", ".", "..", "../secrets", r"C:\Users\player", "1.21.5\n"] {
            assert!(
                parse_install_journal_identity(
                    &[
                        INSTALL_KIND_VANILLA_FACT.to_string(),
                        format!("{INSTALL_VERSION_ID_FACT_PREFIX}{version_id}"),
                    ],
                    version_id,
                )
                .is_none(),
                "unsafe vanilla journal version must be rejected: {version_id:?}"
            );
        }
        let version_id = "1.21.5-pre1";
        assert_eq!(
            parse_install_journal_identity(
                &[
                    INSTALL_KIND_VANILLA_FACT.to_string(),
                    format!("{INSTALL_VERSION_ID_FACT_PREFIX}{version_id}"),
                ],
                version_id,
            ),
            Some(InstallJournalIdentity::vanilla(version_id))
        );
    }

    #[test]
    fn loader_checkpoint_sequences_preserve_base_and_child_authority() {
        let component_id = LoaderComponentId::Fabric;
        let base_version_id = "1.21.5".to_string();
        let target_version_id = installed_version_id_for(component_id, &base_version_id, "0.16.14")
            .expect("canonical loader target");
        let identity = InstallJournalIdentity::Loader {
            target_version_id: target_version_id.clone(),
            component_id,
            build_id: axial_minecraft::build_id_for(component_id, &base_version_id, "0.16.14"),
            base_version_id: base_version_id.clone(),
        };
        let planned = planned_install_journal_for_session(
            &OperationId::deterministic_test("loader-journal-identity"),
            "loader-install-00000000000000000000000000000000",
            &identity,
        );
        let public_target = planned
            .targets
            .iter()
            .find(|target| target.kind == TargetKind::Version)
            .expect("loader journal version target");
        assert_eq!(public_target.id, "target");
        assert_eq!(
            parse_install_journal_identity(
                &planned.planned_steps[0].generated_facts,
                &public_target.id,
            ),
            Some(identity.clone())
        );
        let checkpoint = |kind, version_id: &str| InstallPublicationCheckpoint {
            kind,
            version_id: version_id.to_string(),
            evidence: publication_evidence(version_id),
            activation_contract_id: (kind != InstallPublicationCheckpointKind::RolledBack).then(
                || {
                    axial_minecraft::ManagedInstallActivationContractId::parse(
                        "managed-install-activation-v1.qqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqo",
                    )
                    .expect("canonical test activation contract")
                },
            ),
        };
        let base = checkpoint(
            InstallPublicationCheckpointKind::BaseCommitted,
            &base_version_id,
        );
        let child = checkpoint(
            InstallPublicationCheckpointKind::ChildCommitted,
            &target_version_id,
        );
        let child_rollback = checkpoint(
            InstallPublicationCheckpointKind::RolledBack,
            &target_version_id,
        );
        let base_rollback = checkpoint(
            InstallPublicationCheckpointKind::RolledBack,
            &base_version_id,
        );

        for accepted in [
            vec![],
            vec![base.clone()],
            vec![base_rollback.clone()],
            vec![base.clone(), child.clone()],
            vec![base.clone(), child_rollback.clone()],
        ] {
            assert!(checkpoint_sequence_matches_identity(&accepted, &identity));
        }
        for rejected in [
            vec![child],
            vec![child_rollback],
            vec![base.clone(), base_rollback.clone()],
        ] {
            assert!(!checkpoint_sequence_matches_identity(&rejected, &identity));
        }
        assert!(!checkpoint_sequence_matches_identity(
            &[base.clone(), base_rollback],
            &identity
        ));
        assert!(!checkpoint_sequence_matches_identity(
            &[
                checkpoint(
                    InstallPublicationCheckpointKind::ChildCommitted,
                    &target_version_id,
                ),
                base,
            ],
            &identity
        ));
    }
}
