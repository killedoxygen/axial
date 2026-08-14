use super::model::{
    MAX_OPERATION_EVIDENCE_FACTS, MAX_OPERATION_EVIDENCE_FIELD_KEY_BYTES,
    MAX_OPERATION_EVIDENCE_FIELDS_PER_FACT, MAX_OPERATION_EVIDENCE_SERIALIZED_BYTES,
    MAX_OPERATION_EVIDENCE_TARGET_BYTES, MAX_OPERATION_EVIDENCE_VALUE_BYTES,
};
use super::{
    EvidenceScope, FactReliability, GuardianDomain, GuardianFact, GuardianFactId, GuardianSeverity,
    OperationEvidenceBatchRejection,
};
use crate::execution::{ExecutionFact, ExecutionFactKind};
use crate::observability::{EvidenceField, RedactionAudience, sanitize_evidence_token};
use crate::state::contracts::{OperationId, OperationPhase, OwnershipClass, TargetDescriptor};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OperationEvidenceBatch {
    scope: EvidenceScope,
    facts: Vec<GuardianFact>,
    serialized_bytes: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum EvidenceFactSource {
    Direct,
    Readiness,
    Trusted,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SourcedOperationEvidenceBatch {
    batch: OperationEvidenceBatch,
    sources: Vec<EvidenceFactSource>,
}

impl SourcedOperationEvidenceBatch {
    pub(crate) const fn batch(&self) -> &OperationEvidenceBatch {
        &self.batch
    }

    pub(crate) fn facts_from(
        &self,
        source: EvidenceFactSource,
    ) -> impl Iterator<Item = &GuardianFact> {
        self.batch
            .facts
            .iter()
            .zip(self.sources.iter())
            .filter_map(move |(fact, actual)| (*actual == source).then_some(fact))
    }
}

impl OperationEvidenceBatch {
    pub const fn scope(&self) -> &EvidenceScope {
        &self.scope
    }

    pub fn operation_id(&self) -> Option<&OperationId> {
        match &self.scope {
            EvidenceScope::Operation(operation_id) => Some(operation_id),
            EvidenceScope::Unscoped => None,
        }
    }

    pub fn facts(&self) -> &[GuardianFact] {
        &self.facts
    }

    pub const fn serialized_bytes(&self) -> usize {
        self.serialized_bytes
    }
}

fn guardian_fact_from_execution(fact: &ExecutionFact, phase: OperationPhase) -> GuardianFact {
    let (id, domain, reliability) = execution_fact_shape(fact);
    let target = fact.target.as_ref().map(public_safe_target);
    let ownership = target
        .as_ref()
        .map(|target| target.ownership)
        .unwrap_or(OwnershipClass::Unknown);
    let severity = (fact.kind == ExecutionFactKind::RuntimeMissingExecutable
        && ownership == OwnershipClass::LauncherManaged)
        .then_some(GuardianSeverity::Recoverable);
    GuardianFact {
        operation_id: None,
        id,
        domain,
        phase,
        reliability,
        severity,
        confidence: None,
        ownership,
        target,
        fields: public_safe_fields(&fact.fields),
    }
}

impl OperationEvidenceBatch {
    pub fn try_from_execution_operation(
        operation_id: &OperationId,
        phase: OperationPhase,
        facts: &[ExecutionFact],
    ) -> Result<Self, OperationEvidenceBatchRejection> {
        validate_execution_provenance(facts, &EvidenceScope::Operation(operation_id.clone()))?;
        validate_execution_shapes(facts)?;
        finish_batch(
            EvidenceScope::Operation(operation_id.clone()),
            facts
                .iter()
                .map(|fact| guardian_fact_from_execution(fact, phase))
                .collect(),
        )
    }

    pub fn try_from_execution_unscoped(
        phase: OperationPhase,
        facts: &[ExecutionFact],
    ) -> Result<Self, OperationEvidenceBatchRejection> {
        validate_execution_provenance(facts, &EvidenceScope::Unscoped)?;
        validate_execution_shapes(facts)?;
        finish_batch(
            EvidenceScope::Unscoped,
            facts
                .iter()
                .map(|fact| guardian_fact_from_execution(fact, phase))
                .collect(),
        )
    }

    pub fn try_from_guardian_operation(
        operation_id: &OperationId,
        facts: &[GuardianFact],
    ) -> Result<Self, OperationEvidenceBatchRejection> {
        let scope = EvidenceScope::Operation(operation_id.clone());
        validate_guardian_provenance(facts, &scope, false)?;
        validate_guardian_shapes(facts)?;
        finish_batch(scope, facts.iter().map(normalize_guardian_fact).collect())
    }

    pub(crate) fn try_from_trusted_guardian_operation(
        operation_id: OperationId,
        facts: Vec<GuardianFact>,
    ) -> Result<Self, OperationEvidenceBatchRejection> {
        let scope = EvidenceScope::Operation(operation_id);
        validate_guardian_provenance(&facts, &scope, true)?;
        validate_guardian_shapes(&facts)?;
        finish_batch(scope, facts.iter().map(normalize_guardian_fact).collect())
    }

    pub fn try_from_guardian_unscoped(
        facts: &[GuardianFact],
    ) -> Result<Self, OperationEvidenceBatchRejection> {
        validate_guardian_provenance(facts, &EvidenceScope::Unscoped, false)?;
        validate_guardian_shapes(facts)?;
        finish_batch(
            EvidenceScope::Unscoped,
            facts.iter().map(normalize_guardian_fact).collect(),
        )
    }

    pub(crate) fn try_from_guardian_operation_with_sourced_trusted(
        operation_id: &OperationId,
        direct: &[GuardianFact],
        readiness: &[GuardianFact],
        trusted: Vec<GuardianFact>,
    ) -> Result<SourcedOperationEvidenceBatch, OperationEvidenceBatchRejection> {
        finish_sourced_guardian_batch(
            EvidenceScope::Operation(operation_id.clone()),
            direct,
            readiness,
            trusted,
        )
    }

    pub(crate) fn try_from_guardian_unscoped_with_sourced_trusted(
        direct: &[GuardianFact],
        readiness: &[GuardianFact],
        trusted: Vec<GuardianFact>,
    ) -> Result<SourcedOperationEvidenceBatch, OperationEvidenceBatchRejection> {
        finish_sourced_guardian_batch(EvidenceScope::Unscoped, direct, readiness, trusted)
    }
}

fn finish_sourced_guardian_batch(
    scope: EvidenceScope,
    direct: &[GuardianFact],
    readiness: &[GuardianFact],
    trusted: Vec<GuardianFact>,
) -> Result<SourcedOperationEvidenceBatch, OperationEvidenceBatchRejection> {
    validate_guardian_provenance(direct, &scope, false)?;
    validate_guardian_provenance(readiness, &scope, false)?;
    validate_guardian_provenance(&trusted, &scope, true)?;
    validate_guardian_shapes(direct)?;
    validate_guardian_shapes(readiness)?;
    validate_guardian_shapes(&trusted)?;
    let fact_count = direct
        .len()
        .checked_add(readiness.len())
        .and_then(|count| count.checked_add(trusted.len()))
        .ok_or(OperationEvidenceBatchRejection::TooManyFacts)?;
    validate_fact_count(fact_count)?;

    let facts = direct
        .iter()
        .map(|fact| (normalize_guardian_fact(fact), EvidenceFactSource::Direct))
        .chain(
            readiness
                .iter()
                .map(|fact| (normalize_guardian_fact(fact), EvidenceFactSource::Readiness)),
        )
        .chain(
            trusted
                .iter()
                .map(|fact| (normalize_guardian_fact(fact), EvidenceFactSource::Trusted)),
        )
        .collect::<Vec<_>>();
    let (facts, sources) = distinct_sourced_facts(facts);
    let batch = finish_batch(scope, facts)?;
    debug_assert_eq!(batch.facts.len(), sources.len());
    Ok(SourcedOperationEvidenceBatch { batch, sources })
}

fn validate_execution_provenance(
    facts: &[ExecutionFact],
    scope: &EvidenceScope,
) -> Result<(), OperationEvidenceBatchRejection> {
    validate_fact_count(facts.len())?;
    for (fact_index, fact) in facts.iter().enumerate() {
        match (scope, fact.operation_id.as_ref()) {
            (EvidenceScope::Operation(expected), Some(actual)) if expected == actual => {}
            (EvidenceScope::Operation(_), None) => {
                return Err(OperationEvidenceBatchRejection::MissingOperation { fact_index });
            }
            (EvidenceScope::Operation(_), Some(_)) => {
                return Err(OperationEvidenceBatchRejection::ForeignOperation { fact_index });
            }
            (EvidenceScope::Unscoped, None) => {}
            (EvidenceScope::Unscoped, Some(_)) => {
                return Err(OperationEvidenceBatchRejection::UnexpectedOperation { fact_index });
            }
        }
    }
    Ok(())
}

fn validate_guardian_provenance(
    facts: &[GuardianFact],
    scope: &EvidenceScope,
    trusted: bool,
) -> Result<(), OperationEvidenceBatchRejection> {
    validate_fact_count(facts.len())?;
    for (fact_index, fact) in facts.iter().enumerate() {
        match (scope, trusted, fact.operation_id.as_ref()) {
            (EvidenceScope::Operation(_), true, None) | (EvidenceScope::Unscoped, false, None) => {}
            (EvidenceScope::Operation(expected), false, Some(actual)) if expected == actual => {}
            (EvidenceScope::Operation(_), false, None) => {
                return Err(OperationEvidenceBatchRejection::MissingOperation { fact_index });
            }
            (EvidenceScope::Operation(_), false, Some(_)) => {
                return Err(OperationEvidenceBatchRejection::ForeignOperation { fact_index });
            }
            (EvidenceScope::Operation(_), true, Some(_))
            | (EvidenceScope::Unscoped, _, Some(_)) => {
                return Err(OperationEvidenceBatchRejection::UnexpectedOperation { fact_index });
            }
            (EvidenceScope::Unscoped, true, None) => {}
        }
    }
    Ok(())
}

fn validate_fact_count(count: usize) -> Result<(), OperationEvidenceBatchRejection> {
    if count > MAX_OPERATION_EVIDENCE_FACTS {
        Err(OperationEvidenceBatchRejection::TooManyFacts)
    } else {
        Ok(())
    }
}

fn validate_execution_shapes(
    facts: &[ExecutionFact],
) -> Result<(), OperationEvidenceBatchRejection> {
    for (fact_index, fact) in facts.iter().enumerate() {
        validate_fact_shape(fact_index, fact.target.as_ref(), &fact.fields)?;
    }
    Ok(())
}

fn validate_guardian_shapes(facts: &[GuardianFact]) -> Result<(), OperationEvidenceBatchRejection> {
    validate_fact_count(facts.len())?;
    for (fact_index, fact) in facts.iter().enumerate() {
        validate_fact_shape(fact_index, fact.target.as_ref(), &fact.fields)?;
    }
    Ok(())
}

fn validate_fact_shape(
    fact_index: usize,
    target: Option<&TargetDescriptor>,
    fields: &[EvidenceField],
) -> Result<(), OperationEvidenceBatchRejection> {
    if target.is_some_and(|target| target.id.len() > MAX_OPERATION_EVIDENCE_TARGET_BYTES) {
        return Err(OperationEvidenceBatchRejection::TargetTooLong { fact_index });
    }
    if fields.len() > MAX_OPERATION_EVIDENCE_FIELDS_PER_FACT {
        return Err(OperationEvidenceBatchRejection::TooManyFields { fact_index });
    }
    for (field_index, field) in fields.iter().enumerate() {
        if field.key.len() > MAX_OPERATION_EVIDENCE_FIELD_KEY_BYTES {
            return Err(OperationEvidenceBatchRejection::FieldKeyTooLong {
                fact_index,
                field_index,
            });
        }
        if field.value.len() > MAX_OPERATION_EVIDENCE_VALUE_BYTES {
            return Err(OperationEvidenceBatchRejection::FieldValueTooLong {
                fact_index,
                field_index,
            });
        }
    }
    Ok(())
}

fn normalize_guardian_fact(fact: &GuardianFact) -> GuardianFact {
    GuardianFact {
        operation_id: None,
        id: fact.id,
        domain: fact.domain,
        phase: fact.phase,
        reliability: fact.reliability,
        severity: fact.severity,
        confidence: fact.confidence,
        ownership: fact.ownership,
        target: fact.target.as_ref().map(public_safe_target),
        fields: public_safe_fields(&fact.fields),
    }
}

fn distinct_sourced_facts(
    facts: Vec<(GuardianFact, EvidenceFactSource)>,
) -> (Vec<GuardianFact>, Vec<EvidenceFactSource>) {
    let mut distinct = Vec::with_capacity(facts.len());
    let mut sources = Vec::with_capacity(facts.len());
    for (fact, source) in facts {
        let duplicate = distinct.iter().any(|existing: &GuardianFact| {
            existing.id == fact.id
                && existing.target.as_ref().map(|target| target.id.as_str())
                    == fact.target.as_ref().map(|target| target.id.as_str())
        });
        if !duplicate {
            distinct.push(fact);
            sources.push(source);
        }
    }
    (distinct, sources)
}

fn finish_batch(
    scope: EvidenceScope,
    facts: Vec<GuardianFact>,
) -> Result<OperationEvidenceBatch, OperationEvidenceBatchRejection> {
    validate_guardian_shapes(&facts)?;
    let serialized_bytes = serde_json::to_vec(&(&scope, &facts))
        .map_err(|_| OperationEvidenceBatchRejection::SerializationFailed)?
        .len();
    if serialized_bytes > MAX_OPERATION_EVIDENCE_SERIALIZED_BYTES {
        return Err(OperationEvidenceBatchRejection::SerializedTooLarge);
    }
    Ok(OperationEvidenceBatch {
        scope,
        facts,
        serialized_bytes,
    })
}

fn execution_fact_shape(fact: &ExecutionFact) -> (GuardianFactId, GuardianDomain, FactReliability) {
    let (id, domain) = match fact.kind {
        ExecutionFactKind::ArtifactMissing | ExecutionFactKind::FileMissing => {
            (GuardianFactId::ArtifactMissing, GuardianDomain::Library)
        }
        ExecutionFactKind::DownloadChecksumMismatch => (
            GuardianFactId::ArtifactChecksumMismatch,
            GuardianDomain::Library,
        ),
        ExecutionFactKind::ArtifactHashMismatch => (
            GuardianFactId::ArtifactHashMismatch,
            GuardianDomain::Library,
        ),
        ExecutionFactKind::DownloadSizeMismatch => (
            GuardianFactId::ArtifactSizeMismatch,
            GuardianDomain::Library,
        ),
        ExecutionFactKind::ArtifactSizeDrift => {
            (GuardianFactId::ArtifactSizeDrift, GuardianDomain::Library)
        }
        ExecutionFactKind::DownloadProviderFailure => (
            GuardianFactId::DownloadProviderUnavailable,
            GuardianDomain::Download,
        ),
        ExecutionFactKind::DownloadNetworkFailure | ExecutionFactKind::DownloadInterrupted => (
            GuardianFactId::DownloadInterrupted,
            GuardianDomain::Download,
        ),
        ExecutionFactKind::DownloadTempWriteFailed => (
            GuardianFactId::TempFileWriteFailed,
            GuardianDomain::Filesystem,
        ),
        ExecutionFactKind::DownloadWrittenToTemp => (
            GuardianFactId::DownloadWrittenToTemp,
            GuardianDomain::Download,
        ),
        ExecutionFactKind::DownloadTempDiscarded => (
            GuardianFactId::DownloadTempDiscarded,
            GuardianDomain::Download,
        ),
        ExecutionFactKind::DownloadPromotionFailed => (
            GuardianFactId::AtomicPromotionFailed,
            GuardianDomain::Filesystem,
        ),
        ExecutionFactKind::DownloadPromoted => (
            GuardianFactId::AtomicPromotionCompleted,
            GuardianDomain::Filesystem,
        ),
        ExecutionFactKind::FilePermissionDenied => (
            GuardianFactId::FilesystemPermissionDenied,
            GuardianDomain::Filesystem,
        ),
        ExecutionFactKind::FileQuarantined => {
            (GuardianFactId::ArtifactQuarantined, GuardianDomain::Library)
        }
        ExecutionFactKind::InstallDependencyFailed => (
            GuardianFactId::InstallDependencyFailed,
            GuardianDomain::Install,
        ),
        ExecutionFactKind::InstallExecutionFailed => (
            GuardianFactId::InstallExecutionFailed,
            GuardianDomain::Install,
        ),
        ExecutionFactKind::InstallProcessorFailed => (
            GuardianFactId::InstallProcessorFailed,
            GuardianDomain::Install,
        ),
        ExecutionFactKind::RuntimeCorrupt => (
            GuardianFactId::ManagedRuntimeCorrupt,
            GuardianDomain::Runtime,
        ),
        ExecutionFactKind::RuntimeJavaOverrideEmpty => {
            (GuardianFactId::JavaOverrideEmpty, GuardianDomain::Runtime)
        }
        ExecutionFactKind::RuntimeJavaOverrideUndefinedSentinel => (
            GuardianFactId::JavaOverrideUndefinedSentinel,
            GuardianDomain::Runtime,
        ),
        ExecutionFactKind::RuntimeMissingExecutable => {
            if fact
                .target
                .as_ref()
                .is_some_and(|target| target.ownership == OwnershipClass::UserOwned)
            {
                (GuardianFactId::JavaOverrideMissing, GuardianDomain::Runtime)
            } else {
                (
                    GuardianFactId::ManagedRuntimeMissing,
                    GuardianDomain::Runtime,
                )
            }
        }
        ExecutionFactKind::RuntimeProbeFailed => {
            (GuardianFactId::JavaProbeFailed, GuardianDomain::Runtime)
        }
        ExecutionFactKind::RuntimeReadyMarkerMissing => (
            GuardianFactId::ManagedRuntimeReadyMarkerMissing,
            GuardianDomain::Runtime,
        ),
        ExecutionFactKind::RuntimeRepairApplied => (
            GuardianFactId::ManagedRuntimeRepairApplied,
            GuardianDomain::Runtime,
        ),
        ExecutionFactKind::RuntimeRosettaRequired => (
            GuardianFactId::ManagedRuntimeRosettaRequired,
            GuardianDomain::Runtime,
        ),
        ExecutionFactKind::RuntimeUnavailableForPlatform => (
            GuardianFactId::ManagedRuntimeUnavailableForPlatform,
            GuardianDomain::Runtime,
        ),
        ExecutionFactKind::RuntimeWrongMajor => {
            (GuardianFactId::JavaMajorMismatch, GuardianDomain::Runtime)
        }
        ExecutionFactKind::RuntimeWrongUpdate => {
            (GuardianFactId::JavaUpdateTooOld, GuardianDomain::Runtime)
        }
        ExecutionFactKind::JvmArgsEmpty => (GuardianFactId::JvmArgsEmpty, GuardianDomain::Jvm),
        ExecutionFactKind::JvmArgsParseFailed => {
            (GuardianFactId::JvmArgsParseFailed, GuardianDomain::Jvm)
        }
        ExecutionFactKind::JvmArgReservedLauncherFlag => (
            GuardianFactId::JvmArgReservedLauncherFlag,
            GuardianDomain::Jvm,
        ),
        ExecutionFactKind::JvmArgMemoryConflict => {
            (GuardianFactId::JvmArgMemoryConflict, GuardianDomain::Jvm)
        }
        ExecutionFactKind::JvmArgUnsupportedGc => {
            (GuardianFactId::JvmArgUnsupportedGc, GuardianDomain::Jvm)
        }
        ExecutionFactKind::JvmArgUnlockOrderInvalid => (
            GuardianFactId::JvmArgUnlockOrderInvalid,
            GuardianDomain::Jvm,
        ),
        ExecutionFactKind::JvmArgUnsafeClasspathOverride => (
            GuardianFactId::JvmArgUnsafeClasspathOverride,
            GuardianDomain::Jvm,
        ),
        ExecutionFactKind::JvmArgUnsafeNativePathOverride => (
            GuardianFactId::JvmArgUnsafeNativePathOverride,
            GuardianDomain::Jvm,
        ),
        ExecutionFactKind::JvmArgAgentOverride => {
            (GuardianFactId::JvmArgAgentOverride, GuardianDomain::Jvm)
        }
        ExecutionFactKind::ProcessSpawned => {
            (GuardianFactId::ProcessSpawned, GuardianDomain::Session)
        }
        ExecutionFactKind::ProcessStopIntent => (
            GuardianFactId::LauncherStopRequested,
            GuardianDomain::Session,
        ),
        ExecutionFactKind::ProcessKilled => (process_killed_fact_id(fact), GuardianDomain::Session),
        ExecutionFactKind::ProcessExitCode => (exit_code_fact_id(fact), GuardianDomain::Session),
        ExecutionFactKind::ProcessBootEvidence => {
            (GuardianFactId::BootMarkerObserved, GuardianDomain::Session)
        }
        ExecutionFactKind::ProcessWatchdogAction => {
            (process_watchdog_fact_id(fact), GuardianDomain::Session)
        }
        ExecutionFactKind::ProcessExited => {
            (GuardianFactId::ProcessExited, GuardianDomain::Session)
        }
        ExecutionFactKind::PrimitiveRefused => {
            (GuardianFactId::PrimitiveRefused, GuardianDomain::Unknown)
        }
        ExecutionFactKind::ProviderDataInvalid => {
            (GuardianFactId::ProviderDataInvalid, GuardianDomain::Network)
        }
    };
    (id, domain, reliability_for_execution_fact(fact.kind))
}

fn public_safe_target(target: &TargetDescriptor) -> TargetDescriptor {
    TargetDescriptor::new(
        target.system,
        target.kind,
        target.id.as_str(),
        target.ownership,
    )
}

fn public_safe_fields(fields: &[EvidenceField]) -> Vec<EvidenceField> {
    fields
        .iter()
        .filter_map(|field| {
            field
                .value_for(RedactionAudience::UserVisible)
                .and_then(|value| {
                    sanitize_evidence_token(value, RedactionAudience::UserVisible, 96)
                })
                .map(|value| EvidenceField::new(field.key.clone(), value, field.sensitivity))
        })
        .collect()
}

fn exit_code_fact_id(fact: &ExecutionFact) -> GuardianFactId {
    let exit_code = execution_field(fact, "exit_code").and_then(|value| value.parse::<i32>().ok());
    match exit_code {
        Some(0) => GuardianFactId::ExitCodeZero,
        Some(_) => GuardianFactId::ExitCodeNonzero,
        None => GuardianFactId::ExitCodeUnknown,
    }
}

fn process_killed_fact_id(fact: &ExecutionFact) -> GuardianFactId {
    match execution_field(fact, "reason") {
        Some("startup_watchdog") => GuardianFactId::WatchdogKilledProcess,
        Some(_) | None => GuardianFactId::ProcessKilled,
    }
}

fn process_watchdog_fact_id(fact: &ExecutionFact) -> GuardianFactId {
    match execution_field(fact, "action") {
        Some("startup_no_output_kill") => GuardianFactId::WatchdogKilledProcess,
        Some("startup_window_expired") => GuardianFactId::StartupWindowExpired,
        Some(_) | None => GuardianFactId::WatchdogActionObserved,
    }
}

fn execution_field<'a>(fact: &'a ExecutionFact, key: &str) -> Option<&'a str> {
    fact.fields
        .iter()
        .find(|field| field.key == key)
        .map(|field| field.value.as_str())
}

fn reliability_for_execution_fact(kind: ExecutionFactKind) -> FactReliability {
    match kind {
        ExecutionFactKind::RuntimeProbeFailed
        | ExecutionFactKind::RuntimeRosettaRequired
        | ExecutionFactKind::RuntimeUnavailableForPlatform
        | ExecutionFactKind::RuntimeWrongMajor
        | ExecutionFactKind::RuntimeWrongUpdate
        | ExecutionFactKind::ArtifactHashMismatch
        | ExecutionFactKind::DownloadChecksumMismatch
        | ExecutionFactKind::DownloadSizeMismatch => FactReliability::ValidatedProbe,
        ExecutionFactKind::RuntimeJavaOverrideEmpty
        | ExecutionFactKind::RuntimeJavaOverrideUndefinedSentinel => {
            FactReliability::ExactClassifier
        }
        ExecutionFactKind::JvmArgsParseFailed
        | ExecutionFactKind::JvmArgReservedLauncherFlag
        | ExecutionFactKind::JvmArgMemoryConflict
        | ExecutionFactKind::JvmArgUnsupportedGc
        | ExecutionFactKind::JvmArgUnlockOrderInvalid
        | ExecutionFactKind::JvmArgUnsafeClasspathOverride
        | ExecutionFactKind::JvmArgUnsafeNativePathOverride
        | ExecutionFactKind::JvmArgAgentOverride => FactReliability::ExactClassifier,
        ExecutionFactKind::ProcessSpawned
        | ExecutionFactKind::ProcessStopIntent
        | ExecutionFactKind::ProcessKilled
        | ExecutionFactKind::ProcessExitCode
        | ExecutionFactKind::ProcessBootEvidence
        | ExecutionFactKind::ProcessWatchdogAction
        | ExecutionFactKind::ProcessExited => FactReliability::ProcessLifecycle,
        ExecutionFactKind::RuntimeReadyMarkerMissing => FactReliability::ExpectedMarkerAbsence,
        _ => FactReliability::DirectStructured,
    }
}

#[cfg(test)]
mod contract_tests {
    use super::*;
    use crate::observability::EvidenceSensitivity;
    use crate::state::contracts::{StabilizationSystem, TargetKind};

    fn execution_fact(operation_id: Option<OperationId>) -> ExecutionFact {
        ExecutionFact {
            operation_id,
            kind: ExecutionFactKind::DownloadTempDiscarded,
            target: Some(TargetDescriptor::new(
                StabilizationSystem::Execution,
                TargetKind::Artifact,
                "download_temp",
                OwnershipClass::LauncherManaged,
            )),
            fields: vec![EvidenceField::new(
                "disposition",
                "discarded",
                EvidenceSensitivity::Public,
            )],
        }
    }

    fn guardian_fact(operation_id: Option<OperationId>) -> GuardianFact {
        GuardianFact {
            operation_id,
            id: GuardianFactId::DownloadTempDiscarded,
            domain: GuardianDomain::Download,
            phase: OperationPhase::Downloading,
            reliability: FactReliability::DirectStructured,
            severity: None,
            confidence: None,
            ownership: OwnershipClass::LauncherManaged,
            target: None,
            fields: Vec::new(),
        }
    }

    #[test]
    fn scoped_execution_batch_rejects_the_whole_mixed_provenance_input() {
        let expected = OperationId::deterministic_test("batch-expected");
        let foreign = OperationId::deterministic_test("batch-foreign");
        let facts = [
            execution_fact(Some(expected.clone())),
            execution_fact(Some(foreign)),
            execution_fact(None),
        ];

        assert_eq!(
            OperationEvidenceBatch::try_from_execution_operation(
                &expected,
                OperationPhase::Downloading,
                &facts,
            ),
            Err(OperationEvidenceBatchRejection::ForeignOperation { fact_index: 1 })
        );
    }

    #[test]
    fn scoped_execution_batch_rejects_missing_provenance_without_a_wildcard() {
        let expected = OperationId::deterministic_test("batch-missing");

        assert_eq!(
            OperationEvidenceBatch::try_from_execution_operation(
                &expected,
                OperationPhase::Downloading,
                &[execution_fact(None)],
            ),
            Err(OperationEvidenceBatchRejection::MissingOperation { fact_index: 0 })
        );
    }

    #[test]
    fn accepted_batch_owns_scope_and_removes_redundant_per_fact_ids() {
        let operation_id = OperationId::deterministic_test("batch-canonical");
        let batch = OperationEvidenceBatch::try_from_execution_operation(
            &operation_id,
            OperationPhase::Downloading,
            &[execution_fact(Some(operation_id.clone()))],
        )
        .expect("operation evidence batch");

        assert_eq!(batch.operation_id(), Some(&operation_id));
        assert!(batch.facts().iter().all(|fact| fact.operation_id.is_none()));
        assert_eq!(batch.facts()[0].id, GuardianFactId::DownloadTempDiscarded);
        assert!(batch.serialized_bytes() <= MAX_OPERATION_EVIDENCE_SERIALIZED_BYTES);
    }

    #[test]
    fn trusted_scoped_constructor_rejects_prebound_guardian_facts() {
        let operation_id = OperationId::deterministic_test("batch-trusted");

        assert_eq!(
            OperationEvidenceBatch::try_from_trusted_guardian_operation(
                operation_id.clone(),
                vec![guardian_fact(Some(operation_id))],
            ),
            Err(OperationEvidenceBatchRejection::UnexpectedOperation { fact_index: 0 })
        );
    }

    #[test]
    fn unscoped_constructor_rejects_scoped_input() {
        assert_eq!(
            OperationEvidenceBatch::try_from_guardian_unscoped(&[guardian_fact(Some(
                OperationId::deterministic_test("batch-scoped"),
            ))]),
            Err(OperationEvidenceBatchRejection::UnexpectedOperation { fact_index: 0 })
        );
    }

    #[test]
    fn evidence_shape_caps_reject_instead_of_truncating() {
        let operation_id = OperationId::deterministic_test("batch-caps");
        let mut fact = execution_fact(Some(operation_id.clone()));
        fact.fields[0].key = "k".repeat(MAX_OPERATION_EVIDENCE_FIELD_KEY_BYTES + 1);

        assert_eq!(
            OperationEvidenceBatch::try_from_execution_operation(
                &operation_id,
                OperationPhase::Downloading,
                &[fact],
            ),
            Err(OperationEvidenceBatchRejection::FieldKeyTooLong {
                fact_index: 0,
                field_index: 0,
            })
        );
    }
}
