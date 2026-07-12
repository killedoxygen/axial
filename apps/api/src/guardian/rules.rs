use super::{
    DiagnosisId, GuardianActionKind, GuardianConfidence, GuardianDomain, GuardianFact,
    GuardianFactId, GuardianSeverity,
};
use crate::state::contracts::OperationPhase;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum RuleDomain {
    Fixed(GuardianDomain),
    SupportingFact,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum RuleSeverity {
    Fixed(GuardianSeverity),
    SupportingFactOr(GuardianSeverity),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum RuleConfidence {
    Fixed(GuardianConfidence),
    SupportingFactOr(GuardianConfidence),
    BySource {
        default: GuardianConfidence,
        overrides: &'static [SourceConfidenceOverride],
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct SourceConfidenceOverride {
    fact_id: GuardianFactId,
    confidence: GuardianConfidence,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct RuleClause {
    pub(super) phase: OperationPhase,
    pub(super) required_conditions: &'static [GuardianFactId],
    pub(super) confidence: Option<GuardianConfidence>,
    pub(super) candidate_actions: &'static [GuardianActionKind],
    pub(super) evidence_fact_ids: Option<&'static [GuardianFactId]>,
    pub(super) priority: Option<DecisionPriorityBand>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct RuleSuppression {
    pub(super) phase: OperationPhase,
    pub(super) required_conditions: &'static [GuardianFactId],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct ResolvedDiagnosisRule {
    pub(super) severity: GuardianSeverity,
    pub(super) confidence: GuardianConfidence,
    pub(super) candidate_actions: &'static [GuardianActionKind],
    pub(super) evidence_fact_ids: &'static [GuardianFactId],
    pub(super) priority: DecisionPriorityBand,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(super) enum DecisionPriorityBand {
    UnknownLow,
    RecordHigh,
    RecordConfirmed,
    LaunchBlockLow,
    PerformanceFallbackHigh,
    WarningConfirmed,
    ResourceHigh,
    CustomIntentHigh,
    DegradedHigh,
    DegradedConfirmed,
    LaunchBlockMedium,
    RepairCorruptionHigh,
    RecoverableConfirmed,
    BlockingHighOrPersistedState,
    LaunchBlockingHigh,
    RepairCorruptionConfirmed,
    BlockingConfirmed,
    LaunchBlockingConfirmed,
    OwnershipBoundaryConfirmed,
    Critical,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PriorityProfile {
    Default,
    Record,
    LaunchBlocking,
    RepairCorruption,
    Resource,
    CustomIntent,
    Degraded,
    PerformanceFallback,
    PersistedState,
    OwnershipBoundary,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct DiagnosisRule {
    pub(super) id: DiagnosisId,
    pub(super) trigger_fact_ids: &'static [GuardianFactId],
    pub(super) evidence_fact_ids: &'static [GuardianFactId],
    pub(super) active_phases: &'static [OperationPhase],
    pub(super) required_conditions: &'static [GuardianFactId],
    pub(super) suppressions: &'static [RuleSuppression],
    pub(super) domain: RuleDomain,
    pub(super) severity: RuleSeverity,
    pub(super) confidence: RuleConfidence,
    priority_profile: PriorityProfile,
    pub(super) candidate_actions: &'static [GuardianActionKind],
    pub(super) clauses: &'static [RuleClause],
    pub(super) public_reason_template: &'static str,
}

impl DiagnosisRule {
    pub(super) fn trigger_matches(&self, fact: &GuardianFact, phase: OperationPhase) -> bool {
        self.trigger_fact_ids.contains(&fact.id)
            && (self.active_phases.is_empty() || fact.phase == phase)
    }

    fn has_phase_trigger(&self, facts: &[GuardianFact], phase: OperationPhase) -> bool {
        facts
            .iter()
            .any(|fact| self.trigger_fact_ids.contains(&fact.id) && fact.phase == phase)
    }

    pub(super) fn matches(&self, facts: &[GuardianFact], phase: OperationPhase) -> bool {
        facts.iter().any(|fact| self.trigger_matches(fact, phase))
            && (self.active_phases.is_empty() || self.active_phases.contains(&phase))
            && self
                .required_conditions
                .iter()
                .all(|condition| has_phase_fact(facts, *condition, phase))
            && !self.suppressions.iter().any(|suppression| {
                suppression.phase == phase
                    && self.has_phase_trigger(facts, phase)
                    && suppression
                        .required_conditions
                        .iter()
                        .all(|condition| has_phase_fact(facts, *condition, phase))
            })
    }

    pub(super) fn domain(&self, supporting_facts: &[&GuardianFact]) -> GuardianDomain {
        match self.domain {
            RuleDomain::Fixed(domain) => domain,
            RuleDomain::SupportingFact => {
                supporting_facts
                    .first()
                    .expect("matched diagnosis rule has a supporting fact")
                    .domain
            }
        }
    }

    fn severity(&self, supporting_facts: &[&GuardianFact]) -> GuardianSeverity {
        match self.severity {
            RuleSeverity::Fixed(severity) => severity,
            RuleSeverity::SupportingFactOr(default) => supporting_facts
                .iter()
                .map(|fact| fact.severity.unwrap_or(default))
                .max_by_key(|severity| severity_rank(*severity))
                .unwrap_or(default),
        }
    }

    fn confidence(&self, supporting_facts: &[&GuardianFact]) -> GuardianConfidence {
        match self.confidence {
            RuleConfidence::Fixed(confidence) => confidence,
            RuleConfidence::SupportingFactOr(default) => supporting_facts
                .iter()
                .map(|fact| fact.confidence.unwrap_or(default))
                .max_by_key(|confidence| confidence_rank(*confidence))
                .unwrap_or(default),
            RuleConfidence::BySource { default, overrides } => supporting_facts
                .iter()
                .map(|fact| {
                    overrides
                        .iter()
                        .find_map(|source| (source.fact_id == fact.id).then_some(source.confidence))
                        .unwrap_or(default)
                })
                .max_by_key(|confidence| confidence_rank(*confidence))
                .unwrap_or(default),
        }
    }

    pub(super) fn resolve(
        &self,
        supporting_facts: &[&GuardianFact],
        all_facts: &[GuardianFact],
        phase: OperationPhase,
    ) -> ResolvedDiagnosisRule {
        let clause = self.clauses.iter().find(|clause| {
            clause.phase == phase
                && self.has_phase_trigger(all_facts, phase)
                && clause
                    .required_conditions
                    .iter()
                    .all(|condition| has_phase_fact(all_facts, *condition, phase))
        });
        let severity = self.severity(supporting_facts);
        let confidence = clause
            .and_then(|clause| clause.confidence)
            .unwrap_or_else(|| self.confidence(supporting_facts));
        ResolvedDiagnosisRule {
            severity,
            confidence,
            candidate_actions: clause
                .map(|clause| clause.candidate_actions)
                .unwrap_or(self.candidate_actions),
            evidence_fact_ids: clause
                .and_then(|clause| clause.evidence_fact_ids)
                .unwrap_or(self.evidence_fact_ids),
            priority: clause
                .and_then(|clause| clause.priority)
                .unwrap_or_else(|| priority_band(self.priority_profile, severity, confidence)),
        }
    }
}

fn has_phase_fact(facts: &[GuardianFact], id: GuardianFactId, phase: OperationPhase) -> bool {
    facts
        .iter()
        .any(|fact| fact.id == id && fact.phase == phase)
}

const fn severity_rank(severity: GuardianSeverity) -> u8 {
    match severity {
        GuardianSeverity::Info => 0,
        GuardianSeverity::Warning => 1,
        GuardianSeverity::Degraded => 2,
        GuardianSeverity::Recoverable => 3,
        GuardianSeverity::Repairable => 4,
        GuardianSeverity::Blocking => 5,
        GuardianSeverity::Critical => 6,
    }
}

const fn confidence_rank(confidence: GuardianConfidence) -> u8 {
    match confidence {
        GuardianConfidence::Low => 0,
        GuardianConfidence::Medium => 1,
        GuardianConfidence::High => 2,
        GuardianConfidence::Confirmed => 3,
        GuardianConfidence::Certain => 4,
    }
}

macro_rules! rule {
    (
        $id:ident, [$($fact:ident),+ $(,)?], $domain:expr, $severity:expr,
        $confidence:expr, [$($action:ident),+ $(,)?], $reason:literal
    ) => {
        DiagnosisRule {
            id: DiagnosisId::$id,
            trigger_fact_ids: &[$(GuardianFactId::$fact),+],
            evidence_fact_ids: &[$(GuardianFactId::$fact),+],
            active_phases: &[],
            required_conditions: &[],
            suppressions: &[],
            domain: $domain,
            severity: $severity,
            confidence: $confidence,
            priority_profile: priority_profile(DiagnosisId::$id),
            candidate_actions: &[$(GuardianActionKind::$action),+],
            clauses: &[],
            public_reason_template: $reason,
        }
    };
}

macro_rules! full_rule {
    (
        $id:ident,
        triggers: [$($trigger:ident),+ $(,)?],
        evidence: [$($evidence:ident),+ $(,)?],
        phases: $phases:expr,
        required: $required:expr,
        suppressions: $suppressions:expr,
        $domain:expr, $severity:expr, $confidence:expr,
        [$($action:ident),+ $(,)?],
        clauses: $clauses:expr,
        $reason:literal
    ) => {
        DiagnosisRule {
            id: DiagnosisId::$id,
            trigger_fact_ids: &[$(GuardianFactId::$trigger),+],
            evidence_fact_ids: &[$(GuardianFactId::$evidence),+],
            active_phases: $phases,
            required_conditions: $required,
            suppressions: $suppressions,
            domain: $domain,
            severity: $severity,
            confidence: $confidence,
            priority_profile: priority_profile(DiagnosisId::$id),
            candidate_actions: &[$(GuardianActionKind::$action),+],
            clauses: $clauses,
            public_reason_template: $reason,
        }
    };
}

const fn clause(
    phase: OperationPhase,
    required_conditions: &'static [GuardianFactId],
    confidence: Option<GuardianConfidence>,
    candidate_actions: &'static [GuardianActionKind],
) -> RuleClause {
    RuleClause {
        phase,
        required_conditions,
        confidence,
        candidate_actions,
        evidence_fact_ids: None,
        priority: None,
    }
}

const fn context_clause(
    phase: OperationPhase,
    required_conditions: &'static [GuardianFactId],
    confidence: Option<GuardianConfidence>,
    candidate_actions: &'static [GuardianActionKind],
    evidence_fact_ids: Option<&'static [GuardianFactId]>,
    priority: Option<DecisionPriorityBand>,
) -> RuleClause {
    RuleClause {
        phase,
        required_conditions,
        confidence,
        candidate_actions,
        evidence_fact_ids,
        priority,
    }
}

const fn suppression(
    phase: OperationPhase,
    required_conditions: &'static [GuardianFactId],
) -> RuleSuppression {
    RuleSuppression {
        phase,
        required_conditions,
    }
}

const fn priority_profile(id: DiagnosisId) -> PriorityProfile {
    match id {
        DiagnosisId::JavaOverrideUnavailable
        | DiagnosisId::JavaProbeFailed
        | DiagnosisId::JavaRuntimeMajorMismatch
        | DiagnosisId::JavaRuntimeUpdateTooOld
        | DiagnosisId::ManagedRuntimeUnavailableForPlatform
        | DiagnosisId::ManagedRuntimeRosettaRequired
        | DiagnosisId::InstalledVersionMetadataMissing
        | DiagnosisId::ParentVersionMetadataMissing
        | DiagnosisId::InstallIncomplete
        | DiagnosisId::ClientJarMissing
        | DiagnosisId::LibrariesMissing
        | DiagnosisId::AssetIndexMissing
        | DiagnosisId::LaunchCommandInvalid
        | DiagnosisId::JvmArgUnsupported
        | DiagnosisId::LauncherManagedArtifactSignatureCorrupt
        | DiagnosisId::InstallArtifactMetadataInvalid
        | DiagnosisId::InstallDependencyFailed
        | DiagnosisId::InstallExecutionFailed
        | DiagnosisId::InstallProcessorFailed
        | DiagnosisId::DownloadUnavailable
        | DiagnosisId::FilesystemPermissionDenied
        | DiagnosisId::TempFileLeftover
        | DiagnosisId::AtomicPromotionFailed
        | DiagnosisId::LaunchPrepareFailed
        | DiagnosisId::OutOfMemory
        | DiagnosisId::GraphicsDriverCrash
        | DiagnosisId::MissingDependency
        | DiagnosisId::ModTransformationFailure
        | DiagnosisId::ModAttributedCrash
        | DiagnosisId::ClasspathModuleConflict
        | DiagnosisId::AuthModeIncompatible
        | DiagnosisId::LoaderBootstrapFailure
        | DiagnosisId::StartupFailedUnknown => PriorityProfile::LaunchBlocking,
        DiagnosisId::ManagedRuntimeCorrupt | DiagnosisId::LauncherManagedArtifactCorrupt => {
            PriorityProfile::RepairCorruption
        }
        DiagnosisId::LaunchCommandPrepared
        | DiagnosisId::JvmArgsEmpty
        | DiagnosisId::ProcessLifecycleObserved => PriorityProfile::Record,
        DiagnosisId::LaunchMemoryMinClamped
        | DiagnosisId::LaunchMemoryAllocationLow
        | DiagnosisId::LaunchResourceMemoryPressure
        | DiagnosisId::LaunchResourceCpuPressure
        | DiagnosisId::LaunchResourceInstallPressure
        | DiagnosisId::LaunchResourceDiskPressure => PriorityProfile::Resource,
        DiagnosisId::CustomJavaOverridePresent
        | DiagnosisId::CustomJvmPresetPresent
        | DiagnosisId::CustomJvmArgsPresent => PriorityProfile::CustomIntent,
        DiagnosisId::PerformanceRulesInvalid
        | DiagnosisId::PerformanceHealthDegraded
        | DiagnosisId::PerformanceRepeatedFailureMemory => PriorityProfile::Degraded,
        DiagnosisId::PerformanceFallbackSelected => PriorityProfile::PerformanceFallback,
        DiagnosisId::PersistedStateSchemaInvalid => PriorityProfile::PersistedState,
        DiagnosisId::ArtifactOwnershipUnsafe => PriorityProfile::OwnershipBoundary,
        _ => PriorityProfile::Default,
    }
}

fn priority_band(
    profile: PriorityProfile,
    severity: GuardianSeverity,
    confidence: GuardianConfidence,
) -> DecisionPriorityBand {
    match profile {
        PriorityProfile::Record => match confidence {
            GuardianConfidence::Confirmed | GuardianConfidence::Certain => {
                DecisionPriorityBand::RecordConfirmed
            }
            _ => DecisionPriorityBand::RecordHigh,
        },
        PriorityProfile::LaunchBlocking => match confidence {
            GuardianConfidence::Low => DecisionPriorityBand::LaunchBlockLow,
            GuardianConfidence::Medium => DecisionPriorityBand::LaunchBlockMedium,
            GuardianConfidence::High => DecisionPriorityBand::LaunchBlockingHigh,
            GuardianConfidence::Confirmed | GuardianConfidence::Certain => {
                DecisionPriorityBand::LaunchBlockingConfirmed
            }
        },
        PriorityProfile::RepairCorruption => match confidence {
            GuardianConfidence::Confirmed | GuardianConfidence::Certain => {
                DecisionPriorityBand::RepairCorruptionConfirmed
            }
            _ => DecisionPriorityBand::RepairCorruptionHigh,
        },
        PriorityProfile::Resource => DecisionPriorityBand::ResourceHigh,
        PriorityProfile::CustomIntent => DecisionPriorityBand::CustomIntentHigh,
        PriorityProfile::Degraded => match confidence {
            GuardianConfidence::Confirmed | GuardianConfidence::Certain => {
                DecisionPriorityBand::DegradedConfirmed
            }
            _ => DecisionPriorityBand::DegradedHigh,
        },
        PriorityProfile::PerformanceFallback => DecisionPriorityBand::PerformanceFallbackHigh,
        PriorityProfile::PersistedState => DecisionPriorityBand::BlockingHighOrPersistedState,
        PriorityProfile::OwnershipBoundary => DecisionPriorityBand::OwnershipBoundaryConfirmed,
        PriorityProfile::Default => match severity {
            GuardianSeverity::Info => match confidence {
                GuardianConfidence::Confirmed | GuardianConfidence::Certain => {
                    DecisionPriorityBand::RecordConfirmed
                }
                _ => DecisionPriorityBand::RecordHigh,
            },
            GuardianSeverity::Warning => DecisionPriorityBand::WarningConfirmed,
            GuardianSeverity::Degraded => match confidence {
                GuardianConfidence::Confirmed | GuardianConfidence::Certain => {
                    DecisionPriorityBand::DegradedConfirmed
                }
                _ => DecisionPriorityBand::DegradedHigh,
            },
            GuardianSeverity::Recoverable => DecisionPriorityBand::RecoverableConfirmed,
            GuardianSeverity::Repairable => match confidence {
                GuardianConfidence::Confirmed | GuardianConfidence::Certain => {
                    DecisionPriorityBand::RepairCorruptionConfirmed
                }
                _ => DecisionPriorityBand::RepairCorruptionHigh,
            },
            GuardianSeverity::Blocking => match confidence {
                GuardianConfidence::Low => DecisionPriorityBand::LaunchBlockLow,
                GuardianConfidence::Medium => DecisionPriorityBand::LaunchBlockMedium,
                GuardianConfidence::High => DecisionPriorityBand::BlockingHighOrPersistedState,
                GuardianConfidence::Confirmed | GuardianConfidence::Certain => {
                    DecisionPriorityBand::BlockingConfirmed
                }
            },
            GuardianSeverity::Critical => DecisionPriorityBand::Critical,
        },
    }
}

pub(super) const DIAGNOSIS_RULES: &[DiagnosisRule] = &[
    rule!(
        JavaOverrideUnavailable,
        [
            JavaOverrideEmpty,
            JavaOverrideMissing,
            JavaOverrideUndefinedSentinel,
        ],
        RuleDomain::Fixed(GuardianDomain::Runtime),
        RuleSeverity::Fixed(GuardianSeverity::Blocking),
        RuleConfidence::Fixed(GuardianConfidence::Confirmed),
        [Fallback, AskUser, Block],
        "selected_java_runtime_unavailable"
    ),
    rule!(
        JavaProbeFailed,
        [JavaProbeFailed],
        RuleDomain::Fixed(GuardianDomain::Runtime),
        RuleSeverity::Fixed(GuardianSeverity::Blocking),
        RuleConfidence::Fixed(GuardianConfidence::High),
        [Fallback, Block],
        "java_runtime_probe_failed"
    ),
    full_rule!(
        JavaRuntimeMajorMismatch,
        triggers: [JavaMajorMismatch],
        evidence: [JavaMajorMismatch],
        phases: &[],
        required: &[],
        suppressions: &[],
        RuleDomain::Fixed(GuardianDomain::Runtime),
        RuleSeverity::Fixed(GuardianSeverity::Blocking),
        RuleConfidence::Fixed(GuardianConfidence::Confirmed),
        [Fallback, Block],
        clauses: &[
            clause(
                OperationPhase::Preparing,
                &[
                    GuardianFactId::LaunchFailureClassified,
                    GuardianFactId::LaunchRuntimeFallbackAvailable,
                ],
                Some(GuardianConfidence::Confirmed),
                &[
                    GuardianActionKind::Fallback,
                    GuardianActionKind::AskUser,
                    GuardianActionKind::Block,
                ],
            ),
            context_clause(
                OperationPhase::Launching,
                &[
                    GuardianFactId::LaunchFailureClassified,
                    GuardianFactId::LaunchRuntimeFallbackAvailable,
                    GuardianFactId::ProcessExitedBeforeBoot,
                ],
                Some(GuardianConfidence::High),
                &[GuardianActionKind::Fallback, GuardianActionKind::Block],
                Some(&[
                    GuardianFactId::ProcessExitedBeforeBoot,
                    GuardianFactId::JavaMajorMismatch,
                ]),
                None,
            ),
            context_clause(
                OperationPhase::Launching,
                &[
                    GuardianFactId::LaunchFailureClassified,
                    GuardianFactId::ProcessExitedBeforeBoot,
                ],
                Some(GuardianConfidence::High),
                &[GuardianActionKind::Block],
                Some(&[
                    GuardianFactId::ProcessExitedBeforeBoot,
                    GuardianFactId::JavaMajorMismatch,
                ]),
                None,
            ),
            clause(
                OperationPhase::Launching,
                &[GuardianFactId::LaunchFailureClassified],
                Some(GuardianConfidence::High),
                &[GuardianActionKind::Block],
            ),
            clause(
                OperationPhase::Preparing,
                &[GuardianFactId::LaunchFailureClassified],
                Some(GuardianConfidence::Confirmed),
                &[GuardianActionKind::Block],
            ),
        ],
        "java_runtime_major_mismatch"
    ),
    rule!(
        JavaRuntimeUpdateTooOld,
        [JavaUpdateTooOld],
        RuleDomain::Fixed(GuardianDomain::Runtime),
        RuleSeverity::Fixed(GuardianSeverity::Blocking),
        RuleConfidence::Fixed(GuardianConfidence::Confirmed),
        [Fallback, Block],
        "java_update_too_old"
    ),
    rule!(
        ManagedRuntimeMissing,
        [ManagedRuntimeMissing],
        RuleDomain::Fixed(GuardianDomain::Runtime),
        RuleSeverity::SupportingFactOr(GuardianSeverity::Recoverable),
        RuleConfidence::SupportingFactOr(GuardianConfidence::Confirmed),
        [RecordOnly],
        "managed_runtime_missing"
    ),
    rule!(
        ManagedRuntimeUnavailableForPlatform,
        [ManagedRuntimeUnavailableForPlatform],
        RuleDomain::Fixed(GuardianDomain::Runtime),
        RuleSeverity::Fixed(GuardianSeverity::Blocking),
        RuleConfidence::Fixed(GuardianConfidence::Confirmed),
        [Block],
        "managed_runtime_unavailable_for_platform"
    ),
    rule!(
        ManagedRuntimeRosettaRequired,
        [ManagedRuntimeRosettaRequired],
        RuleDomain::Fixed(GuardianDomain::Runtime),
        RuleSeverity::Fixed(GuardianSeverity::Blocking),
        RuleConfidence::Fixed(GuardianConfidence::Confirmed),
        [Block],
        "managed_runtime_rosetta_required"
    ),
    rule!(
        ManagedRuntimeCorrupt,
        [ManagedRuntimeReadyMarkerMissing, ManagedRuntimeCorrupt],
        RuleDomain::Fixed(GuardianDomain::Runtime),
        RuleSeverity::Fixed(GuardianSeverity::Repairable),
        RuleConfidence::Fixed(GuardianConfidence::Confirmed),
        [Repair, Block],
        "managed_runtime_needs_repair"
    ),
    rule!(
        InstalledVersionMetadataMissing,
        [VersionJsonMissing],
        RuleDomain::Fixed(GuardianDomain::Install),
        RuleSeverity::SupportingFactOr(GuardianSeverity::Blocking),
        RuleConfidence::SupportingFactOr(GuardianConfidence::Confirmed),
        [Block],
        "version_json_missing"
    ),
    rule!(
        ParentVersionMetadataMissing,
        [ParentVersionMissing],
        RuleDomain::Fixed(GuardianDomain::Install),
        RuleSeverity::SupportingFactOr(GuardianSeverity::Blocking),
        RuleConfidence::SupportingFactOr(GuardianConfidence::Confirmed),
        [Block],
        "parent_version_missing"
    ),
    rule!(
        InstallIncomplete,
        [IncompleteInstall],
        RuleDomain::Fixed(GuardianDomain::Install),
        RuleSeverity::SupportingFactOr(GuardianSeverity::Blocking),
        RuleConfidence::SupportingFactOr(GuardianConfidence::Confirmed),
        [Block],
        "incomplete_install"
    ),
    rule!(
        ClientJarMissing,
        [ClientJarMissing],
        RuleDomain::Fixed(GuardianDomain::Install),
        RuleSeverity::SupportingFactOr(GuardianSeverity::Blocking),
        RuleConfidence::SupportingFactOr(GuardianConfidence::Confirmed),
        [Block],
        "client_jar_missing"
    ),
    rule!(
        LibrariesMissing,
        [LibrariesMissing],
        RuleDomain::Fixed(GuardianDomain::Install),
        RuleSeverity::SupportingFactOr(GuardianSeverity::Blocking),
        RuleConfidence::SupportingFactOr(GuardianConfidence::Confirmed),
        [Block],
        "libraries_missing"
    ),
    rule!(
        AssetIndexMissing,
        [AssetIndexMissing],
        RuleDomain::Fixed(GuardianDomain::Install),
        RuleSeverity::SupportingFactOr(GuardianSeverity::Blocking),
        RuleConfidence::SupportingFactOr(GuardianConfidence::Confirmed),
        [Block],
        "asset_index_missing"
    ),
    rule!(
        LaunchCommandInvalid,
        [LaunchCommandInvalid],
        RuleDomain::Fixed(GuardianDomain::Launch),
        RuleSeverity::Fixed(GuardianSeverity::Blocking),
        RuleConfidence::Fixed(GuardianConfidence::Confirmed),
        [Block],
        "launch_command_invalid"
    ),
    rule!(
        LaunchCommandPrepared,
        [LaunchCommandPrepared],
        RuleDomain::Fixed(GuardianDomain::Launch),
        RuleSeverity::Fixed(GuardianSeverity::Info),
        RuleConfidence::Fixed(GuardianConfidence::Confirmed),
        [RecordOnly],
        "launch_command_prepared"
    ),
    rule!(
        LaunchMemoryMinClamped,
        [LaunchMemoryMinClamped],
        RuleDomain::SupportingFact,
        RuleSeverity::SupportingFactOr(GuardianSeverity::Warning),
        RuleConfidence::SupportingFactOr(GuardianConfidence::High),
        [Warn, RecordOnly],
        "launch_memory_min_clamped"
    ),
    rule!(
        LaunchMemoryAllocationLow,
        [LaunchMemoryAllocationLow],
        RuleDomain::SupportingFact,
        RuleSeverity::SupportingFactOr(GuardianSeverity::Warning),
        RuleConfidence::SupportingFactOr(GuardianConfidence::High),
        [Warn, RecordOnly],
        "launch_memory_allocation_low"
    ),
    rule!(
        LaunchResourceMemoryPressure,
        [LaunchResourceMemoryPressure],
        RuleDomain::SupportingFact,
        RuleSeverity::SupportingFactOr(GuardianSeverity::Warning),
        RuleConfidence::SupportingFactOr(GuardianConfidence::High),
        [Warn, RecordOnly],
        "launch_resource_memory_pressure"
    ),
    rule!(
        LaunchResourceCpuPressure,
        [LaunchResourceCpuPressure],
        RuleDomain::SupportingFact,
        RuleSeverity::SupportingFactOr(GuardianSeverity::Warning),
        RuleConfidence::SupportingFactOr(GuardianConfidence::High),
        [Warn, RecordOnly],
        "launch_resource_cpu_pressure"
    ),
    rule!(
        LaunchResourceInstallPressure,
        [LaunchResourceInstallPressure],
        RuleDomain::SupportingFact,
        RuleSeverity::SupportingFactOr(GuardianSeverity::Warning),
        RuleConfidence::SupportingFactOr(GuardianConfidence::High),
        [Warn, RecordOnly],
        "launch_resource_install_pressure"
    ),
    rule!(
        LaunchResourceDiskPressure,
        [LaunchResourceDiskPressure],
        RuleDomain::SupportingFact,
        RuleSeverity::SupportingFactOr(GuardianSeverity::Warning),
        RuleConfidence::SupportingFactOr(GuardianConfidence::High),
        [Warn, RecordOnly],
        "launch_resource_disk_pressure"
    ),
    rule!(
        CustomJavaOverridePresent,
        [CustomJavaOverridePresent],
        RuleDomain::SupportingFact,
        RuleSeverity::SupportingFactOr(GuardianSeverity::Warning),
        RuleConfidence::SupportingFactOr(GuardianConfidence::High),
        [Warn, RecordOnly],
        "custom_java_override_present"
    ),
    rule!(
        CustomJvmPresetPresent,
        [CustomJvmPresetPresent],
        RuleDomain::SupportingFact,
        RuleSeverity::SupportingFactOr(GuardianSeverity::Warning),
        RuleConfidence::SupportingFactOr(GuardianConfidence::High),
        [Warn, RecordOnly],
        "custom_jvm_preset_present"
    ),
    rule!(
        CustomJvmArgsPresent,
        [CustomJvmArgsPresent],
        RuleDomain::SupportingFact,
        RuleSeverity::SupportingFactOr(GuardianSeverity::Warning),
        RuleConfidence::SupportingFactOr(GuardianConfidence::High),
        [Warn, RecordOnly],
        "custom_jvm_args_present"
    ),
    rule!(
        JvmArgsEmpty,
        [JvmArgsEmpty],
        RuleDomain::Fixed(GuardianDomain::Jvm),
        RuleSeverity::Fixed(GuardianSeverity::Info),
        RuleConfidence::Fixed(GuardianConfidence::Confirmed),
        [RecordOnly],
        "jvm_args_empty"
    ),
    rule!(
        JvmArgsMalformed,
        [JvmArgsParseFailed],
        RuleDomain::Fixed(GuardianDomain::Jvm),
        RuleSeverity::Fixed(GuardianSeverity::Blocking),
        RuleConfidence::Fixed(GuardianConfidence::Confirmed),
        [Strip, AskUser, Block],
        "jvm_args_malformed"
    ),
    full_rule!(
        JvmArgUnsupported,
        triggers: [
            JvmArgUnsupportedGc,
            JvmArgUnlockOrderInvalid,
            JvmArgUnsupported,
            JvmArgExperimentalUnlockMissing,
        ],
        evidence: [
            JvmArgUnsupportedGc,
            JvmArgUnlockOrderInvalid,
            JvmArgUnsupported,
            JvmArgExperimentalUnlockMissing,
        ],
        phases: &[],
        required: &[],
        suppressions: &[],
        RuleDomain::Fixed(GuardianDomain::Jvm),
        RuleSeverity::Fixed(GuardianSeverity::Blocking),
        RuleConfidence::Fixed(GuardianConfidence::Confirmed),
        [Strip, AskUser, Block],
        clauses: &[
            clause(
                OperationPhase::Preparing,
                &[
                    GuardianFactId::LaunchFailureClassified,
                    GuardianFactId::LaunchJvmStripAvailable,
                ],
                Some(GuardianConfidence::Confirmed),
                &[
                    GuardianActionKind::Strip,
                    GuardianActionKind::AskUser,
                    GuardianActionKind::Block,
                ],
            ),
            context_clause(
                OperationPhase::Launching,
                &[
                    GuardianFactId::LaunchFailureClassified,
                    GuardianFactId::LaunchJvmPresetDowngradeAvailable,
                    GuardianFactId::ProcessExitedBeforeBoot,
                ],
                Some(GuardianConfidence::High),
                &[GuardianActionKind::Downgrade, GuardianActionKind::Block],
                Some(&[
                    GuardianFactId::ProcessExitedBeforeBoot,
                    GuardianFactId::JvmArgUnsupportedGc,
                    GuardianFactId::JvmArgUnlockOrderInvalid,
                    GuardianFactId::JvmArgUnsupported,
                    GuardianFactId::JvmArgExperimentalUnlockMissing,
                ]),
                None,
            ),
            context_clause(
                OperationPhase::Launching,
                &[
                    GuardianFactId::LaunchFailureClassified,
                    GuardianFactId::LaunchJvmStripAvailable,
                    GuardianFactId::ProcessExitedBeforeBoot,
                ],
                Some(GuardianConfidence::High),
                &[GuardianActionKind::Strip, GuardianActionKind::Block],
                Some(&[
                    GuardianFactId::ProcessExitedBeforeBoot,
                    GuardianFactId::JvmArgUnsupportedGc,
                    GuardianFactId::JvmArgUnlockOrderInvalid,
                    GuardianFactId::JvmArgUnsupported,
                    GuardianFactId::JvmArgExperimentalUnlockMissing,
                ]),
                None,
            ),
            context_clause(
                OperationPhase::Launching,
                &[
                    GuardianFactId::LaunchFailureClassified,
                    GuardianFactId::ProcessExitedBeforeBoot,
                ],
                Some(GuardianConfidence::High),
                &[GuardianActionKind::Block],
                Some(&[
                    GuardianFactId::ProcessExitedBeforeBoot,
                    GuardianFactId::JvmArgUnsupportedGc,
                    GuardianFactId::JvmArgUnlockOrderInvalid,
                    GuardianFactId::JvmArgUnsupported,
                    GuardianFactId::JvmArgExperimentalUnlockMissing,
                ]),
                None,
            ),
            clause(
                OperationPhase::Launching,
                &[GuardianFactId::LaunchFailureClassified],
                Some(GuardianConfidence::High),
                &[GuardianActionKind::Block],
            ),
            clause(
                OperationPhase::Preparing,
                &[GuardianFactId::LaunchFailureClassified],
                Some(GuardianConfidence::Confirmed),
                &[GuardianActionKind::Block],
            ),
        ],
        "jvm_arg_unsupported"
    ),
    rule!(
        JvmArgUnsafeOverride,
        [
            JvmArgReservedLauncherFlag,
            JvmArgMemoryConflict,
            JvmArgUnsafeClasspathOverride,
            JvmArgUnsafeNativePathOverride,
            JvmArgAgentOverride,
        ],
        RuleDomain::Fixed(GuardianDomain::Jvm),
        RuleSeverity::Fixed(GuardianSeverity::Blocking),
        RuleConfidence::Fixed(GuardianConfidence::Confirmed),
        [Strip, AskUser, Block],
        "jvm_arg_unsafe_override"
    ),
    full_rule!(
        LauncherManagedArtifactSignatureCorrupt,
        triggers: [LauncherManagedArtifactSignatureCorruption],
        evidence: [LauncherManagedArtifactSignatureCorruption],
        phases: &[],
        required: &[],
        suppressions: &[suppression(
            OperationPhase::Preparing,
            &[GuardianFactId::LaunchFailureClassified],
        )],
        RuleDomain::Fixed(GuardianDomain::Download),
        RuleSeverity::Fixed(GuardianSeverity::Blocking),
        RuleConfidence::Fixed(GuardianConfidence::Confirmed),
        [Block],
        clauses: &[context_clause(
            OperationPhase::Launching,
            &[
                GuardianFactId::LaunchFailureClassified,
                GuardianFactId::ProcessExitedBeforeBoot,
            ],
            Some(GuardianConfidence::High),
            &[GuardianActionKind::Block],
            Some(&[
                GuardianFactId::ProcessExitedBeforeBoot,
                GuardianFactId::LauncherManagedArtifactSignatureCorruption,
            ]),
            None,
        )],
        "launcher_managed_artifact_signature_corrupt"
    ),
    rule!(
        LauncherManagedArtifactCorrupt,
        [
            ArtifactChecksumMismatch,
            ArtifactSizeMismatch,
            ManagedFileCorrupt,
            ArtifactMissing,
        ],
        RuleDomain::SupportingFact,
        RuleSeverity::Fixed(GuardianSeverity::Repairable),
        RuleConfidence::BySource {
            default: GuardianConfidence::Confirmed,
            overrides: &[SourceConfidenceOverride {
                fact_id: GuardianFactId::ArtifactMissing,
                confidence: GuardianConfidence::High,
            }],
        },
        [Quarantine, Repair, Block],
        "managed_artifact_corrupt"
    ),
    rule!(
        InstallArtifactMetadataInvalid,
        [ProviderDataInvalid],
        RuleDomain::Fixed(GuardianDomain::Install),
        RuleSeverity::Fixed(GuardianSeverity::Blocking),
        RuleConfidence::Fixed(GuardianConfidence::Confirmed),
        [Block],
        "install_artifact_metadata_invalid"
    ),
    rule!(
        InstallDependencyFailed,
        [InstallDependencyFailed],
        RuleDomain::Fixed(GuardianDomain::Install),
        RuleSeverity::Fixed(GuardianSeverity::Blocking),
        RuleConfidence::Fixed(GuardianConfidence::Confirmed),
        [Block],
        "install_dependency_failed"
    ),
    rule!(
        InstallExecutionFailed,
        [InstallExecutionFailed],
        RuleDomain::Fixed(GuardianDomain::Install),
        RuleSeverity::Fixed(GuardianSeverity::Blocking),
        RuleConfidence::Fixed(GuardianConfidence::Confirmed),
        [Block],
        "install_execution_failed"
    ),
    rule!(
        InstallProcessorFailed,
        [InstallProcessorFailed],
        RuleDomain::Fixed(GuardianDomain::Install),
        RuleSeverity::Fixed(GuardianSeverity::Blocking),
        RuleConfidence::Fixed(GuardianConfidence::Confirmed),
        [Block],
        "install_processor_failed"
    ),
    rule!(
        DownloadUnavailable,
        [DownloadProviderUnavailable, DownloadInterrupted],
        RuleDomain::Fixed(GuardianDomain::Download),
        RuleSeverity::Fixed(GuardianSeverity::Blocking),
        RuleConfidence::Fixed(GuardianConfidence::Medium),
        [Retry, AskUser, Block],
        "download_unavailable"
    ),
    rule!(
        FilesystemPermissionDenied,
        [FilesystemPermissionDenied],
        RuleDomain::Fixed(GuardianDomain::Filesystem),
        RuleSeverity::Fixed(GuardianSeverity::Blocking),
        RuleConfidence::Fixed(GuardianConfidence::Confirmed),
        [Block],
        "filesystem_permission_denied"
    ),
    rule!(
        TempFileLeftover,
        [TempFileLeftover],
        RuleDomain::Fixed(GuardianDomain::Filesystem),
        RuleSeverity::Fixed(GuardianSeverity::Blocking),
        RuleConfidence::Fixed(GuardianConfidence::Confirmed),
        [Block],
        "temp_file_leftover"
    ),
    rule!(
        AtomicPromotionFailed,
        [AtomicPromotionFailed],
        RuleDomain::Fixed(GuardianDomain::Filesystem),
        RuleSeverity::Fixed(GuardianSeverity::Blocking),
        RuleConfidence::Fixed(GuardianConfidence::Confirmed),
        [Block],
        "atomic_promotion_failed"
    ),
    rule!(
        ArtifactOwnershipUnsafe,
        [OwnershipUnknown, PrimitiveRefused],
        RuleDomain::Fixed(GuardianDomain::Filesystem),
        RuleSeverity::Fixed(GuardianSeverity::Blocking),
        RuleConfidence::Fixed(GuardianConfidence::Confirmed),
        [Block],
        "artifact_ownership_unsafe"
    ),
    rule!(
        PerformanceRulesInvalid,
        [PerformanceRulesInvalid],
        RuleDomain::Fixed(GuardianDomain::Performance),
        RuleSeverity::SupportingFactOr(GuardianSeverity::Degraded),
        RuleConfidence::SupportingFactOr(GuardianConfidence::High),
        [RecordOnly, Warn],
        "performance_rules_invalid"
    ),
    rule!(
        PerformanceHealthDegraded,
        [PerformanceHealthDegraded],
        RuleDomain::Fixed(GuardianDomain::Performance),
        RuleSeverity::SupportingFactOr(GuardianSeverity::Degraded),
        RuleConfidence::SupportingFactOr(GuardianConfidence::High),
        [RecordOnly, Warn],
        "performance_health_degraded"
    ),
    rule!(
        PerformanceHealthInvalid,
        [PerformanceHealthInvalid],
        RuleDomain::Fixed(GuardianDomain::Performance),
        RuleSeverity::SupportingFactOr(GuardianSeverity::Degraded),
        RuleConfidence::SupportingFactOr(GuardianConfidence::High),
        [RecordOnly, Warn],
        "performance_health_invalid"
    ),
    rule!(
        PerformanceFallbackSelected,
        [PerformanceFallbackSelected, PerformanceHealthFallback],
        RuleDomain::Fixed(GuardianDomain::Performance),
        RuleSeverity::SupportingFactOr(GuardianSeverity::Warning),
        RuleConfidence::SupportingFactOr(GuardianConfidence::High),
        [RecordOnly, Warn],
        "performance_fallback_selected"
    ),
    rule!(
        PerformanceRepeatedFailureMemory,
        [PerformanceRepeatedFailureMemory],
        RuleDomain::Fixed(GuardianDomain::Performance),
        RuleSeverity::SupportingFactOr(GuardianSeverity::Degraded),
        RuleConfidence::SupportingFactOr(GuardianConfidence::High),
        [RecordOnly, Warn],
        "performance_repeated_failure_memory"
    ),
    rule!(
        PerformanceUserOwnedConflict,
        [PerformanceUserOwnedConflict],
        RuleDomain::Fixed(GuardianDomain::Performance),
        RuleSeverity::SupportingFactOr(GuardianSeverity::Blocking),
        RuleConfidence::SupportingFactOr(GuardianConfidence::Confirmed),
        [RecordOnly, Warn, AskUser, Block],
        "performance_user_owned_conflict"
    ),
    full_rule!(
        ProcessLifecycleObserved,
        triggers: [
            ProcessSpawned,
            LauncherStopRequested,
            WatchdogKilledProcess,
            ExitCodeZero,
            ExitCodeNonzero,
            ExitCodeUnknown,
            BootMarkerObserved,
            ProcessExited,
            ProcessExitedBeforeBoot,
            ProcessExitedAfterBoot,
        ],
        evidence: [
            ProcessSpawned,
            LauncherStopRequested,
            WatchdogKilledProcess,
            ExitCodeZero,
            ExitCodeNonzero,
            ExitCodeUnknown,
            BootMarkerObserved,
            ProcessExited,
            ProcessExitedBeforeBoot,
            ProcessExitedAfterBoot,
        ],
        phases: &[],
        required: &[],
        suppressions: &[suppression(
            OperationPhase::Launching,
            &[
                GuardianFactId::LaunchFailureClassified,
                GuardianFactId::ProcessExitedBeforeBoot,
            ],
        )],
        RuleDomain::Fixed(GuardianDomain::Session),
        RuleSeverity::Fixed(GuardianSeverity::Info),
        RuleConfidence::Fixed(GuardianConfidence::High),
        [RecordOnly],
        clauses: &[],
        "process_lifecycle_observed"
    ),
    rule!(
        PersistedStateSchemaInvalid,
        [PersistedStateSchemaInvalid],
        RuleDomain::Fixed(GuardianDomain::State),
        RuleSeverity::Fixed(GuardianSeverity::Warning),
        RuleConfidence::Fixed(GuardianConfidence::Confirmed),
        [Warn, RecordOnly],
        "persisted_state_schema_invalid"
    ),
    full_rule!(
        JvmPresetAdjusted,
        triggers: [JvmPresetCompatibilityAdjusted],
        evidence: [JvmPresetCompatibilityAdjusted],
        phases: &[OperationPhase::Preparing],
        required: &[],
        suppressions: &[],
        RuleDomain::Fixed(GuardianDomain::Jvm),
        RuleSeverity::Fixed(GuardianSeverity::Recoverable),
        RuleConfidence::Fixed(GuardianConfidence::Confirmed),
        [Downgrade, AskUser, Block],
        clauses: &[],
        "jvm_preset_adjusted"
    ),
    full_rule!(
        LaunchPrepareFailed,
        triggers: [
            OutOfMemory,
            GraphicsDriverCrash,
            MissingDependency,
            ModTransformationFailure,
            ModAttributedCrash,
            ClasspathModuleConflict,
            LauncherManagedArtifactSignatureCorruption,
            AuthModeIncompatible,
            LoaderBootstrapFailure,
            StartupWindowExpired,
            UnknownLaunchFailure,
        ],
        evidence: [
            OutOfMemory,
            GraphicsDriverCrash,
            MissingDependency,
            ModTransformationFailure,
            ModAttributedCrash,
            ClasspathModuleConflict,
            LauncherManagedArtifactSignatureCorruption,
            AuthModeIncompatible,
            LoaderBootstrapFailure,
            StartupWindowExpired,
            UnknownLaunchFailure,
        ],
        phases: &[OperationPhase::Preparing],
        required: &[GuardianFactId::LaunchFailureClassified],
        suppressions: &[],
        RuleDomain::Fixed(GuardianDomain::Launch),
        RuleSeverity::Fixed(GuardianSeverity::Blocking),
        RuleConfidence::BySource {
            default: GuardianConfidence::High,
            overrides: &[SourceConfidenceOverride {
                fact_id: GuardianFactId::UnknownLaunchFailure,
                confidence: GuardianConfidence::Low,
            }],
        },
        [Block],
        clauses: &[],
        "launch_prepare_failed"
    ),
    full_rule!(
        StartupStalled,
        triggers: [StartupWindowExpired],
        evidence: [StartupWindowExpired],
        phases: &[OperationPhase::Launching],
        required: &[GuardianFactId::LaunchFailureClassified],
        suppressions: &[],
        RuleDomain::Fixed(GuardianDomain::Startup),
        RuleSeverity::Fixed(GuardianSeverity::Blocking),
        RuleConfidence::Fixed(GuardianConfidence::High),
        [Block],
        clauses: &[context_clause(
            OperationPhase::Launching,
            &[GuardianFactId::ProcessExitedBeforeBoot],
            None,
            &[GuardianActionKind::Block],
            Some(&[
                GuardianFactId::ProcessExitedBeforeBoot,
                GuardianFactId::StartupWindowExpired,
            ]),
            Some(DecisionPriorityBand::LaunchBlockingHigh),
        )],
        "startup_stalled"
    ),
    full_rule!(
        OutOfMemory,
        triggers: [OutOfMemory],
        evidence: [ProcessExitedBeforeBoot, OutOfMemory],
        phases: &[OperationPhase::Launching],
        required: &[
            GuardianFactId::LaunchFailureClassified,
            GuardianFactId::ProcessExitedBeforeBoot,
        ],
        suppressions: &[],
        RuleDomain::Fixed(GuardianDomain::Startup),
        RuleSeverity::Fixed(GuardianSeverity::Blocking),
        RuleConfidence::Fixed(GuardianConfidence::High),
        [Block],
        clauses: &[],
        "out_of_memory"
    ),
    full_rule!(
        GraphicsDriverCrash,
        triggers: [GraphicsDriverCrash],
        evidence: [ProcessExitedBeforeBoot, GraphicsDriverCrash],
        phases: &[OperationPhase::Launching],
        required: &[
            GuardianFactId::LaunchFailureClassified,
            GuardianFactId::ProcessExitedBeforeBoot,
        ],
        suppressions: &[],
        RuleDomain::Fixed(GuardianDomain::Startup),
        RuleSeverity::Fixed(GuardianSeverity::Blocking),
        RuleConfidence::Fixed(GuardianConfidence::High),
        [Block],
        clauses: &[],
        "graphics_driver_crash"
    ),
    full_rule!(
        MissingDependency,
        triggers: [MissingDependency],
        evidence: [ProcessExitedBeforeBoot, MissingDependency],
        phases: &[OperationPhase::Launching],
        required: &[
            GuardianFactId::LaunchFailureClassified,
            GuardianFactId::ProcessExitedBeforeBoot,
        ],
        suppressions: &[],
        RuleDomain::Fixed(GuardianDomain::Startup),
        RuleSeverity::Fixed(GuardianSeverity::Blocking),
        RuleConfidence::Fixed(GuardianConfidence::High),
        [Block],
        clauses: &[],
        "missing_dependency"
    ),
    full_rule!(
        ModTransformationFailure,
        triggers: [ModTransformationFailure],
        evidence: [ProcessExitedBeforeBoot, ModTransformationFailure],
        phases: &[OperationPhase::Launching],
        required: &[
            GuardianFactId::LaunchFailureClassified,
            GuardianFactId::ProcessExitedBeforeBoot,
        ],
        suppressions: &[],
        RuleDomain::Fixed(GuardianDomain::Startup),
        RuleSeverity::Fixed(GuardianSeverity::Blocking),
        RuleConfidence::Fixed(GuardianConfidence::High),
        [Block],
        clauses: &[],
        "mod_transformation_failure"
    ),
    full_rule!(
        ModAttributedCrash,
        triggers: [ModAttributedCrash],
        evidence: [ProcessExitedBeforeBoot, ModAttributedCrash],
        phases: &[OperationPhase::Launching],
        required: &[
            GuardianFactId::LaunchFailureClassified,
            GuardianFactId::ProcessExitedBeforeBoot,
        ],
        suppressions: &[],
        RuleDomain::Fixed(GuardianDomain::Startup),
        RuleSeverity::Fixed(GuardianSeverity::Blocking),
        RuleConfidence::Fixed(GuardianConfidence::High),
        [Block],
        clauses: &[],
        "mod_attributed_crash"
    ),
    full_rule!(
        ClasspathModuleConflict,
        triggers: [ClasspathModuleConflict],
        evidence: [ProcessExitedBeforeBoot, ClasspathModuleConflict],
        phases: &[OperationPhase::Launching],
        required: &[
            GuardianFactId::LaunchFailureClassified,
            GuardianFactId::ProcessExitedBeforeBoot,
        ],
        suppressions: &[],
        RuleDomain::Fixed(GuardianDomain::Startup),
        RuleSeverity::Fixed(GuardianSeverity::Blocking),
        RuleConfidence::Fixed(GuardianConfidence::High),
        [Block],
        clauses: &[],
        "classpath_module_conflict"
    ),
    full_rule!(
        AuthModeIncompatible,
        triggers: [AuthModeIncompatible],
        evidence: [ProcessExitedBeforeBoot, AuthModeIncompatible],
        phases: &[OperationPhase::Launching],
        required: &[
            GuardianFactId::LaunchFailureClassified,
            GuardianFactId::ProcessExitedBeforeBoot,
        ],
        suppressions: &[],
        RuleDomain::Fixed(GuardianDomain::Startup),
        RuleSeverity::Fixed(GuardianSeverity::Blocking),
        RuleConfidence::Fixed(GuardianConfidence::High),
        [Block],
        clauses: &[],
        "auth_mode_incompatible"
    ),
    full_rule!(
        LoaderBootstrapFailure,
        triggers: [LoaderBootstrapFailure],
        evidence: [ProcessExitedBeforeBoot, LoaderBootstrapFailure],
        phases: &[OperationPhase::Launching],
        required: &[
            GuardianFactId::LaunchFailureClassified,
            GuardianFactId::ProcessExitedBeforeBoot,
        ],
        suppressions: &[],
        RuleDomain::Fixed(GuardianDomain::Startup),
        RuleSeverity::Fixed(GuardianSeverity::Blocking),
        RuleConfidence::Fixed(GuardianConfidence::High),
        [Block],
        clauses: &[],
        "loader_bootstrap_failure"
    ),
    full_rule!(
        StartupFailedUnknown,
        triggers: [UnknownLaunchFailure],
        evidence: [ProcessExitedBeforeBoot, UnknownLaunchFailure],
        phases: &[OperationPhase::Launching],
        required: &[
            GuardianFactId::LaunchFailureClassified,
            GuardianFactId::ProcessExitedBeforeBoot,
        ],
        suppressions: &[],
        RuleDomain::Fixed(GuardianDomain::Startup),
        RuleSeverity::Fixed(GuardianSeverity::Blocking),
        RuleConfidence::Fixed(GuardianConfidence::Low),
        [Block],
        clauses: &[],
        "startup_failed_unknown"
    ),
];

#[cfg(test)]
pub(super) fn rule_for_diagnosis(id: DiagnosisId) -> Option<&'static DiagnosisRule> {
    DIAGNOSIS_RULES.iter().find(|rule| rule.id == id)
}

pub(super) fn rule_order(id: DiagnosisId) -> Option<usize> {
    DIAGNOSIS_RULES.iter().position(|rule| rule.id == id)
}
