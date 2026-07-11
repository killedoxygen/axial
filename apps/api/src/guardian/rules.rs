use super::{
    DiagnosisId, GuardianActionKind, GuardianConfidence, GuardianDomain, GuardianFact,
    GuardianFactId, GuardianImpactVector, GuardianSeverity,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum OwnershipRequirement {
    None,
    Classified,
    LauncherManaged,
    CompositionManaged,
    UserOrUnknownProtected,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum JournalRequirement {
    None,
    RequiredForAttemptAction,
    RequiredForManagedMutation,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum RedactionRequirement {
    PublicOutcome,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum RetryLoopSensitivity {
    None,
    OneAttemptOverride,
    RepairAttempt,
    ProviderRetry,
    RepeatedFailureMemory,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum DestructiveMutationRisk {
    None,
    ManagedMutation,
    UserOrUnknownProtected,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum UserIntentSensitivity {
    None,
    ExplicitTechnicalIntent,
    PerformanceComposition,
    UserDataBoundary,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct ActionEligibility {
    pub(super) ownership_requirement: OwnershipRequirement,
    pub(super) journal_requirement: JournalRequirement,
    pub(super) redaction_requirement: RedactionRequirement,
    pub(super) retry_loop_sensitivity: RetryLoopSensitivity,
    pub(super) destructive_mutation_risk: DestructiveMutationRisk,
    pub(super) user_intent_sensitivity: UserIntentSensitivity,
}

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
enum RuleImpact {
    LaunchBlocking,
    RepairableCorruption,
    RecordOnly,
    JavaOverrideUnavailable,
    ManagedRuntimeMissing,
    Readiness,
    ResourcePressure,
    CustomIntent,
    JvmArgsMalformed,
    UnsafeJvmOverride,
    ArtifactOwnershipUnsafe,
    PerformanceRulesInvalid,
    PerformanceHealth,
    PerformanceFallback,
    PerformanceRepeatedFailure,
    PerformanceUserOwnedConflict,
    PersistedStateSchemaInvalid,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct DiagnosisRule {
    pub(super) id: DiagnosisId,
    pub(super) source_fact_ids: &'static [GuardianFactId],
    pub(super) domain: RuleDomain,
    pub(super) severity: RuleSeverity,
    pub(super) confidence: RuleConfidence,
    impact: RuleImpact,
    pub(super) eligibility: ActionEligibility,
    pub(super) candidate_actions: &'static [GuardianActionKind],
    pub(super) public_reason_template: &'static str,
}

impl DiagnosisRule {
    pub(super) fn matches(&self, fact: &GuardianFact) -> bool {
        self.source_fact_ids.contains(&fact.id)
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

    pub(super) fn severity(&self, supporting_facts: &[&GuardianFact]) -> GuardianSeverity {
        match self.severity {
            RuleSeverity::Fixed(severity) => severity,
            RuleSeverity::SupportingFactOr(default) => supporting_facts
                .iter()
                .map(|fact| fact.severity.unwrap_or(default))
                .max_by_key(|severity| severity_rank(*severity))
                .unwrap_or(default),
        }
    }

    pub(super) fn confidence(&self, supporting_facts: &[&GuardianFact]) -> GuardianConfidence {
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

    pub(super) fn impact(&self) -> GuardianImpactVector {
        match self.impact {
            RuleImpact::LaunchBlocking => GuardianImpactVector::launch_blocking(),
            RuleImpact::RepairableCorruption => GuardianImpactVector::repairable_corruption(),
            RuleImpact::RecordOnly => GuardianImpactVector::record_only(),
            RuleImpact::JavaOverrideUnavailable => GuardianImpactVector {
                launchability_impact: 0.95,
                user_intent_impact: 0.65,
                ..GuardianImpactVector::default()
            },
            RuleImpact::ManagedRuntimeMissing => GuardianImpactVector {
                launchability_impact: 0.35,
                state_corruption_impact: 0.10,
                ..GuardianImpactVector::default()
            },
            RuleImpact::Readiness => GuardianImpactVector {
                launchability_impact: 0.95,
                state_corruption_impact: readiness_state_corruption_impact(self.id),
                ..GuardianImpactVector::default()
            },
            RuleImpact::ResourcePressure => GuardianImpactVector {
                launchability_impact: 0.35,
                performance_impact: 0.45,
                host_stability_impact: 0.50,
                ..GuardianImpactVector::default()
            },
            RuleImpact::CustomIntent => GuardianImpactVector {
                user_intent_impact: 0.55,
                launchability_impact: 0.20,
                ..GuardianImpactVector::default()
            },
            RuleImpact::JvmArgsMalformed => GuardianImpactVector {
                launchability_impact: 0.90,
                user_intent_impact: 0.70,
                ..GuardianImpactVector::default()
            },
            RuleImpact::UnsafeJvmOverride => GuardianImpactVector {
                launchability_impact: 0.90,
                user_intent_impact: 0.75,
                host_stability_impact: 0.60,
                ..GuardianImpactVector::default()
            },
            RuleImpact::ArtifactOwnershipUnsafe => GuardianImpactVector {
                data_loss_risk: 0.95,
                user_intent_impact: 0.80,
                launchability_impact: 0.70,
                ..GuardianImpactVector::default()
            },
            RuleImpact::PerformanceRulesInvalid => GuardianImpactVector {
                launchability_impact: 0.25,
                performance_impact: 0.80,
                ..GuardianImpactVector::default()
            },
            RuleImpact::PerformanceHealth => GuardianImpactVector {
                launchability_impact: 0.35,
                state_corruption_impact: if self.id == DiagnosisId::PerformanceHealthInvalid {
                    0.75
                } else {
                    0.35
                },
                performance_impact: 0.80,
                ..GuardianImpactVector::default()
            },
            RuleImpact::PerformanceFallback => GuardianImpactVector {
                launchability_impact: 0.15,
                performance_impact: 0.60,
                ..GuardianImpactVector::default()
            },
            RuleImpact::PerformanceRepeatedFailure => GuardianImpactVector {
                launchability_impact: 0.30,
                performance_impact: 0.75,
                ..GuardianImpactVector::default()
            },
            RuleImpact::PerformanceUserOwnedConflict => GuardianImpactVector {
                data_loss_risk: 0.75,
                user_intent_impact: 0.85,
                performance_impact: 0.45,
                ..GuardianImpactVector::default()
            },
            RuleImpact::PersistedStateSchemaInvalid => GuardianImpactVector {
                launchability_impact: 0.25,
                state_corruption_impact: 0.80,
                ..GuardianImpactVector::default()
            },
        }
    }
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

const RECORD_ONLY_ELIGIBILITY: ActionEligibility = ActionEligibility {
    ownership_requirement: OwnershipRequirement::None,
    journal_requirement: JournalRequirement::None,
    redaction_requirement: RedactionRequirement::PublicOutcome,
    retry_loop_sensitivity: RetryLoopSensitivity::None,
    destructive_mutation_risk: DestructiveMutationRisk::None,
    user_intent_sensitivity: UserIntentSensitivity::None,
};
const BLOCK_ONLY_ELIGIBILITY: ActionEligibility = ActionEligibility {
    ownership_requirement: OwnershipRequirement::Classified,
    ..RECORD_ONLY_ELIGIBILITY
};
const RUNTIME_ATTEMPT_ELIGIBILITY: ActionEligibility = ActionEligibility {
    ownership_requirement: OwnershipRequirement::Classified,
    journal_requirement: JournalRequirement::RequiredForAttemptAction,
    retry_loop_sensitivity: RetryLoopSensitivity::OneAttemptOverride,
    user_intent_sensitivity: UserIntentSensitivity::ExplicitTechnicalIntent,
    ..RECORD_ONLY_ELIGIBILITY
};
const MANAGED_RUNTIME_REPAIR_ELIGIBILITY: ActionEligibility = ActionEligibility {
    ownership_requirement: OwnershipRequirement::LauncherManaged,
    journal_requirement: JournalRequirement::RequiredForManagedMutation,
    retry_loop_sensitivity: RetryLoopSensitivity::RepairAttempt,
    destructive_mutation_risk: DestructiveMutationRisk::ManagedMutation,
    ..RECORD_ONLY_ELIGIBILITY
};
const EXPLICIT_INTENT_WARNING_ELIGIBILITY: ActionEligibility = ActionEligibility {
    ownership_requirement: OwnershipRequirement::Classified,
    user_intent_sensitivity: UserIntentSensitivity::ExplicitTechnicalIntent,
    ..RECORD_ONLY_ELIGIBILITY
};
const JVM_ATTEMPT_ELIGIBILITY: ActionEligibility = RUNTIME_ATTEMPT_ELIGIBILITY;
const MANAGED_ARTIFACT_REPAIR_ELIGIBILITY: ActionEligibility = MANAGED_RUNTIME_REPAIR_ELIGIBILITY;
const PROVIDER_RETRY_ELIGIBILITY: ActionEligibility = ActionEligibility {
    ownership_requirement: OwnershipRequirement::Classified,
    journal_requirement: JournalRequirement::RequiredForAttemptAction,
    retry_loop_sensitivity: RetryLoopSensitivity::ProviderRetry,
    ..RECORD_ONLY_ELIGIBILITY
};
const USER_OR_UNKNOWN_PROTECTION_ELIGIBILITY: ActionEligibility = ActionEligibility {
    ownership_requirement: OwnershipRequirement::UserOrUnknownProtected,
    destructive_mutation_risk: DestructiveMutationRisk::UserOrUnknownProtected,
    user_intent_sensitivity: UserIntentSensitivity::UserDataBoundary,
    ..RECORD_ONLY_ELIGIBILITY
};
const PERFORMANCE_RECORD_ELIGIBILITY: ActionEligibility = ActionEligibility {
    ownership_requirement: OwnershipRequirement::CompositionManaged,
    user_intent_sensitivity: UserIntentSensitivity::PerformanceComposition,
    ..RECORD_ONLY_ELIGIBILITY
};
const PERFORMANCE_MEMORY_ELIGIBILITY: ActionEligibility = ActionEligibility {
    retry_loop_sensitivity: RetryLoopSensitivity::RepeatedFailureMemory,
    ..PERFORMANCE_RECORD_ELIGIBILITY
};
const PERFORMANCE_USER_CONFLICT_ELIGIBILITY: ActionEligibility = ActionEligibility {
    ownership_requirement: OwnershipRequirement::UserOrUnknownProtected,
    destructive_mutation_risk: DestructiveMutationRisk::UserOrUnknownProtected,
    user_intent_sensitivity: UserIntentSensitivity::UserDataBoundary,
    ..RECORD_ONLY_ELIGIBILITY
};

macro_rules! rule {
    (
        $id:ident, [$($fact:ident),+ $(,)?], $domain:expr, $severity:expr,
        $confidence:expr, $eligibility:ident, [$($action:ident),+ $(,)?], $reason:literal
    ) => {
        DiagnosisRule {
            id: DiagnosisId::$id,
            source_fact_ids: &[$(GuardianFactId::$fact),+],
            domain: $domain,
            severity: $severity,
            confidence: $confidence,
            impact: rule_impact(DiagnosisId::$id),
            eligibility: $eligibility,
            candidate_actions: &[$(GuardianActionKind::$action),+],
            public_reason_template: $reason,
        }
    };
}

const fn rule_impact(id: DiagnosisId) -> RuleImpact {
    match id {
        DiagnosisId::JavaOverrideUnavailable => RuleImpact::JavaOverrideUnavailable,
        DiagnosisId::ManagedRuntimeMissing => RuleImpact::ManagedRuntimeMissing,
        DiagnosisId::ManagedRuntimeCorrupt | DiagnosisId::LauncherManagedArtifactCorrupt => {
            RuleImpact::RepairableCorruption
        }
        DiagnosisId::InstalledVersionMetadataMissing
        | DiagnosisId::ParentVersionMetadataMissing
        | DiagnosisId::InstallIncomplete
        | DiagnosisId::ClientJarMissing
        | DiagnosisId::LibrariesMissing
        | DiagnosisId::AssetIndexMissing => RuleImpact::Readiness,
        DiagnosisId::LaunchCommandPrepared
        | DiagnosisId::JvmArgsEmpty
        | DiagnosisId::ProcessLifecycleObserved => RuleImpact::RecordOnly,
        DiagnosisId::LaunchMemoryMinClamped
        | DiagnosisId::LaunchMemoryAllocationLow
        | DiagnosisId::LaunchResourceMemoryPressure
        | DiagnosisId::LaunchResourceCpuPressure
        | DiagnosisId::LaunchResourceInstallPressure
        | DiagnosisId::LaunchResourceDiskPressure => RuleImpact::ResourcePressure,
        DiagnosisId::CustomJavaOverridePresent
        | DiagnosisId::CustomJvmPresetPresent
        | DiagnosisId::CustomJvmArgsPresent => RuleImpact::CustomIntent,
        DiagnosisId::JvmArgsMalformed => RuleImpact::JvmArgsMalformed,
        DiagnosisId::JvmArgUnsafeOverride => RuleImpact::UnsafeJvmOverride,
        DiagnosisId::ArtifactOwnershipUnsafe => RuleImpact::ArtifactOwnershipUnsafe,
        DiagnosisId::PerformanceRulesInvalid => RuleImpact::PerformanceRulesInvalid,
        DiagnosisId::PerformanceHealthDegraded | DiagnosisId::PerformanceHealthInvalid => {
            RuleImpact::PerformanceHealth
        }
        DiagnosisId::PerformanceFallbackSelected => RuleImpact::PerformanceFallback,
        DiagnosisId::PerformanceRepeatedFailureMemory => RuleImpact::PerformanceRepeatedFailure,
        DiagnosisId::PerformanceUserOwnedConflict => RuleImpact::PerformanceUserOwnedConflict,
        DiagnosisId::PersistedStateSchemaInvalid => RuleImpact::PersistedStateSchemaInvalid,
        DiagnosisId::JavaProbeFailed
        | DiagnosisId::JavaRuntimeMajorMismatch
        | DiagnosisId::JavaRuntimeUpdateTooOld
        | DiagnosisId::ManagedRuntimeUnavailableForPlatform
        | DiagnosisId::ManagedRuntimeRosettaRequired
        | DiagnosisId::LaunchCommandInvalid
        | DiagnosisId::JvmArgUnsupported
        | DiagnosisId::LauncherManagedArtifactSignatureCorrupt
        | DiagnosisId::InstallArtifactMetadataInvalid
        | DiagnosisId::InstallDependencyFailed
        | DiagnosisId::DownloadUnavailable
        | DiagnosisId::FilesystemPermissionDenied
        | DiagnosisId::TempFileLeftover
        | DiagnosisId::AtomicPromotionFailed => RuleImpact::LaunchBlocking,
        _ => panic!("diagnosis rule missing impact metadata"),
    }
}

fn readiness_state_corruption_impact(id: DiagnosisId) -> f32 {
    match id {
        DiagnosisId::InstallIncomplete => 0.75,
        DiagnosisId::InstalledVersionMetadataMissing
        | DiagnosisId::ParentVersionMetadataMissing => 0.65,
        DiagnosisId::ClientJarMissing
        | DiagnosisId::LibrariesMissing
        | DiagnosisId::AssetIndexMissing => 0.55,
        _ => 0.50,
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
        RUNTIME_ATTEMPT_ELIGIBILITY,
        [Fallback, AskUser, Block],
        "selected_java_runtime_unavailable"
    ),
    rule!(
        JavaProbeFailed,
        [JavaProbeFailed],
        RuleDomain::Fixed(GuardianDomain::Runtime),
        RuleSeverity::Fixed(GuardianSeverity::Blocking),
        RuleConfidence::Fixed(GuardianConfidence::High),
        RUNTIME_ATTEMPT_ELIGIBILITY,
        [Fallback, Block],
        "java_runtime_probe_failed"
    ),
    rule!(
        JavaRuntimeMajorMismatch,
        [JavaMajorMismatch],
        RuleDomain::Fixed(GuardianDomain::Runtime),
        RuleSeverity::Fixed(GuardianSeverity::Blocking),
        RuleConfidence::Fixed(GuardianConfidence::Confirmed),
        RUNTIME_ATTEMPT_ELIGIBILITY,
        [Fallback, Block],
        "java_runtime_major_mismatch"
    ),
    rule!(
        JavaRuntimeUpdateTooOld,
        [JavaUpdateTooOld],
        RuleDomain::Fixed(GuardianDomain::Runtime),
        RuleSeverity::Fixed(GuardianSeverity::Blocking),
        RuleConfidence::Fixed(GuardianConfidence::Confirmed),
        RUNTIME_ATTEMPT_ELIGIBILITY,
        [Fallback, Block],
        "java_update_too_old"
    ),
    rule!(
        ManagedRuntimeMissing,
        [ManagedRuntimeMissing],
        RuleDomain::Fixed(GuardianDomain::Runtime),
        RuleSeverity::SupportingFactOr(GuardianSeverity::Recoverable),
        RuleConfidence::SupportingFactOr(GuardianConfidence::Confirmed),
        RECORD_ONLY_ELIGIBILITY,
        [RecordOnly],
        "managed_runtime_missing"
    ),
    rule!(
        ManagedRuntimeUnavailableForPlatform,
        [ManagedRuntimeUnavailableForPlatform],
        RuleDomain::Fixed(GuardianDomain::Runtime),
        RuleSeverity::Fixed(GuardianSeverity::Blocking),
        RuleConfidence::Fixed(GuardianConfidence::Confirmed),
        BLOCK_ONLY_ELIGIBILITY,
        [Block],
        "managed_runtime_unavailable_for_platform"
    ),
    rule!(
        ManagedRuntimeRosettaRequired,
        [ManagedRuntimeRosettaRequired],
        RuleDomain::Fixed(GuardianDomain::Runtime),
        RuleSeverity::Fixed(GuardianSeverity::Blocking),
        RuleConfidence::Fixed(GuardianConfidence::Confirmed),
        BLOCK_ONLY_ELIGIBILITY,
        [Block],
        "managed_runtime_rosetta_required"
    ),
    rule!(
        ManagedRuntimeCorrupt,
        [ManagedRuntimeReadyMarkerMissing, ManagedRuntimeCorrupt],
        RuleDomain::Fixed(GuardianDomain::Runtime),
        RuleSeverity::Fixed(GuardianSeverity::Repairable),
        RuleConfidence::Fixed(GuardianConfidence::Confirmed),
        MANAGED_RUNTIME_REPAIR_ELIGIBILITY,
        [Repair, Block],
        "managed_runtime_needs_repair"
    ),
    rule!(
        InstalledVersionMetadataMissing,
        [VersionJsonMissing],
        RuleDomain::Fixed(GuardianDomain::Install),
        RuleSeverity::SupportingFactOr(GuardianSeverity::Blocking),
        RuleConfidence::SupportingFactOr(GuardianConfidence::Confirmed),
        BLOCK_ONLY_ELIGIBILITY,
        [Block],
        "version_json_missing"
    ),
    rule!(
        ParentVersionMetadataMissing,
        [ParentVersionMissing],
        RuleDomain::Fixed(GuardianDomain::Install),
        RuleSeverity::SupportingFactOr(GuardianSeverity::Blocking),
        RuleConfidence::SupportingFactOr(GuardianConfidence::Confirmed),
        BLOCK_ONLY_ELIGIBILITY,
        [Block],
        "parent_version_missing"
    ),
    rule!(
        InstallIncomplete,
        [IncompleteInstall],
        RuleDomain::Fixed(GuardianDomain::Install),
        RuleSeverity::SupportingFactOr(GuardianSeverity::Blocking),
        RuleConfidence::SupportingFactOr(GuardianConfidence::Confirmed),
        BLOCK_ONLY_ELIGIBILITY,
        [Block],
        "incomplete_install"
    ),
    rule!(
        ClientJarMissing,
        [ClientJarMissing],
        RuleDomain::Fixed(GuardianDomain::Install),
        RuleSeverity::SupportingFactOr(GuardianSeverity::Blocking),
        RuleConfidence::SupportingFactOr(GuardianConfidence::Confirmed),
        BLOCK_ONLY_ELIGIBILITY,
        [Block],
        "client_jar_missing"
    ),
    rule!(
        LibrariesMissing,
        [LibrariesMissing],
        RuleDomain::Fixed(GuardianDomain::Install),
        RuleSeverity::SupportingFactOr(GuardianSeverity::Blocking),
        RuleConfidence::SupportingFactOr(GuardianConfidence::Confirmed),
        BLOCK_ONLY_ELIGIBILITY,
        [Block],
        "libraries_missing"
    ),
    rule!(
        AssetIndexMissing,
        [AssetIndexMissing],
        RuleDomain::Fixed(GuardianDomain::Install),
        RuleSeverity::SupportingFactOr(GuardianSeverity::Blocking),
        RuleConfidence::SupportingFactOr(GuardianConfidence::Confirmed),
        BLOCK_ONLY_ELIGIBILITY,
        [Block],
        "asset_index_missing"
    ),
    rule!(
        LaunchCommandInvalid,
        [LaunchCommandInvalid],
        RuleDomain::Fixed(GuardianDomain::Launch),
        RuleSeverity::Fixed(GuardianSeverity::Blocking),
        RuleConfidence::Fixed(GuardianConfidence::Confirmed),
        BLOCK_ONLY_ELIGIBILITY,
        [Block],
        "launch_command_invalid"
    ),
    rule!(
        LaunchCommandPrepared,
        [LaunchCommandPrepared],
        RuleDomain::Fixed(GuardianDomain::Launch),
        RuleSeverity::Fixed(GuardianSeverity::Info),
        RuleConfidence::Fixed(GuardianConfidence::Confirmed),
        RECORD_ONLY_ELIGIBILITY,
        [RecordOnly],
        "launch_command_prepared"
    ),
    rule!(
        LaunchMemoryMinClamped,
        [LaunchMemoryMinClamped],
        RuleDomain::SupportingFact,
        RuleSeverity::SupportingFactOr(GuardianSeverity::Warning),
        RuleConfidence::SupportingFactOr(GuardianConfidence::High),
        RECORD_ONLY_ELIGIBILITY,
        [Warn, RecordOnly],
        "launch_memory_min_clamped"
    ),
    rule!(
        LaunchMemoryAllocationLow,
        [LaunchMemoryAllocationLow],
        RuleDomain::SupportingFact,
        RuleSeverity::SupportingFactOr(GuardianSeverity::Warning),
        RuleConfidence::SupportingFactOr(GuardianConfidence::High),
        RECORD_ONLY_ELIGIBILITY,
        [Warn, RecordOnly],
        "launch_memory_allocation_low"
    ),
    rule!(
        LaunchResourceMemoryPressure,
        [LaunchResourceMemoryPressure],
        RuleDomain::SupportingFact,
        RuleSeverity::SupportingFactOr(GuardianSeverity::Warning),
        RuleConfidence::SupportingFactOr(GuardianConfidence::High),
        RECORD_ONLY_ELIGIBILITY,
        [Warn, RecordOnly],
        "launch_resource_memory_pressure"
    ),
    rule!(
        LaunchResourceCpuPressure,
        [LaunchResourceCpuPressure],
        RuleDomain::SupportingFact,
        RuleSeverity::SupportingFactOr(GuardianSeverity::Warning),
        RuleConfidence::SupportingFactOr(GuardianConfidence::High),
        RECORD_ONLY_ELIGIBILITY,
        [Warn, RecordOnly],
        "launch_resource_cpu_pressure"
    ),
    rule!(
        LaunchResourceInstallPressure,
        [LaunchResourceInstallPressure],
        RuleDomain::SupportingFact,
        RuleSeverity::SupportingFactOr(GuardianSeverity::Warning),
        RuleConfidence::SupportingFactOr(GuardianConfidence::High),
        RECORD_ONLY_ELIGIBILITY,
        [Warn, RecordOnly],
        "launch_resource_install_pressure"
    ),
    rule!(
        LaunchResourceDiskPressure,
        [LaunchResourceDiskPressure],
        RuleDomain::SupportingFact,
        RuleSeverity::SupportingFactOr(GuardianSeverity::Warning),
        RuleConfidence::SupportingFactOr(GuardianConfidence::High),
        RECORD_ONLY_ELIGIBILITY,
        [Warn, RecordOnly],
        "launch_resource_disk_pressure"
    ),
    rule!(
        CustomJavaOverridePresent,
        [CustomJavaOverridePresent],
        RuleDomain::SupportingFact,
        RuleSeverity::SupportingFactOr(GuardianSeverity::Warning),
        RuleConfidence::SupportingFactOr(GuardianConfidence::High),
        EXPLICIT_INTENT_WARNING_ELIGIBILITY,
        [Warn, RecordOnly],
        "custom_java_override_present"
    ),
    rule!(
        CustomJvmPresetPresent,
        [CustomJvmPresetPresent],
        RuleDomain::SupportingFact,
        RuleSeverity::SupportingFactOr(GuardianSeverity::Warning),
        RuleConfidence::SupportingFactOr(GuardianConfidence::High),
        EXPLICIT_INTENT_WARNING_ELIGIBILITY,
        [Warn, RecordOnly],
        "custom_jvm_preset_present"
    ),
    rule!(
        CustomJvmArgsPresent,
        [CustomJvmArgsPresent],
        RuleDomain::SupportingFact,
        RuleSeverity::SupportingFactOr(GuardianSeverity::Warning),
        RuleConfidence::SupportingFactOr(GuardianConfidence::High),
        EXPLICIT_INTENT_WARNING_ELIGIBILITY,
        [Warn, RecordOnly],
        "custom_jvm_args_present"
    ),
    rule!(
        JvmArgsEmpty,
        [JvmArgsEmpty],
        RuleDomain::Fixed(GuardianDomain::Jvm),
        RuleSeverity::Fixed(GuardianSeverity::Info),
        RuleConfidence::Fixed(GuardianConfidence::Confirmed),
        RECORD_ONLY_ELIGIBILITY,
        [RecordOnly],
        "jvm_args_empty"
    ),
    rule!(
        JvmArgsMalformed,
        [JvmArgsParseFailed],
        RuleDomain::Fixed(GuardianDomain::Jvm),
        RuleSeverity::Fixed(GuardianSeverity::Blocking),
        RuleConfidence::Fixed(GuardianConfidence::Confirmed),
        JVM_ATTEMPT_ELIGIBILITY,
        [Strip, AskUser, Block],
        "jvm_args_malformed"
    ),
    rule!(
        JvmArgUnsupported,
        [JvmArgUnsupportedGc, JvmArgUnlockOrderInvalid],
        RuleDomain::Fixed(GuardianDomain::Jvm),
        RuleSeverity::Fixed(GuardianSeverity::Blocking),
        RuleConfidence::Fixed(GuardianConfidence::Confirmed),
        JVM_ATTEMPT_ELIGIBILITY,
        [Strip, AskUser, Block],
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
        JVM_ATTEMPT_ELIGIBILITY,
        [Strip, AskUser, Block],
        "jvm_arg_unsafe_override"
    ),
    rule!(
        LauncherManagedArtifactSignatureCorrupt,
        [LauncherManagedArtifactSignatureCorruption],
        RuleDomain::SupportingFact,
        RuleSeverity::Fixed(GuardianSeverity::Blocking),
        RuleConfidence::Fixed(GuardianConfidence::Confirmed),
        BLOCK_ONLY_ELIGIBILITY,
        [Block],
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
        MANAGED_ARTIFACT_REPAIR_ELIGIBILITY,
        [Quarantine, Repair, Block],
        "managed_artifact_corrupt"
    ),
    rule!(
        InstallArtifactMetadataInvalid,
        [ProviderDataInvalid],
        RuleDomain::Fixed(GuardianDomain::Install),
        RuleSeverity::Fixed(GuardianSeverity::Blocking),
        RuleConfidence::Fixed(GuardianConfidence::Confirmed),
        BLOCK_ONLY_ELIGIBILITY,
        [Block],
        "install_artifact_metadata_invalid"
    ),
    rule!(
        InstallDependencyFailed,
        [InstallDependencyFailed],
        RuleDomain::Fixed(GuardianDomain::Install),
        RuleSeverity::Fixed(GuardianSeverity::Blocking),
        RuleConfidence::Fixed(GuardianConfidence::Confirmed),
        BLOCK_ONLY_ELIGIBILITY,
        [Block],
        "install_dependency_failed"
    ),
    rule!(
        DownloadUnavailable,
        [DownloadProviderUnavailable, DownloadInterrupted],
        RuleDomain::Fixed(GuardianDomain::Download),
        RuleSeverity::Fixed(GuardianSeverity::Blocking),
        RuleConfidence::Fixed(GuardianConfidence::Medium),
        PROVIDER_RETRY_ELIGIBILITY,
        [Retry, AskUser, Block],
        "download_unavailable"
    ),
    rule!(
        FilesystemPermissionDenied,
        [FilesystemPermissionDenied],
        RuleDomain::Fixed(GuardianDomain::Filesystem),
        RuleSeverity::Fixed(GuardianSeverity::Blocking),
        RuleConfidence::Fixed(GuardianConfidence::Confirmed),
        BLOCK_ONLY_ELIGIBILITY,
        [Block],
        "filesystem_permission_denied"
    ),
    rule!(
        TempFileLeftover,
        [TempFileLeftover],
        RuleDomain::Fixed(GuardianDomain::Filesystem),
        RuleSeverity::Fixed(GuardianSeverity::Blocking),
        RuleConfidence::Fixed(GuardianConfidence::Confirmed),
        BLOCK_ONLY_ELIGIBILITY,
        [Block],
        "temp_file_leftover"
    ),
    rule!(
        AtomicPromotionFailed,
        [AtomicPromotionFailed],
        RuleDomain::Fixed(GuardianDomain::Filesystem),
        RuleSeverity::Fixed(GuardianSeverity::Blocking),
        RuleConfidence::Fixed(GuardianConfidence::Confirmed),
        BLOCK_ONLY_ELIGIBILITY,
        [Block],
        "atomic_promotion_failed"
    ),
    rule!(
        ArtifactOwnershipUnsafe,
        [OwnershipUnknown, PrimitiveRefused],
        RuleDomain::Fixed(GuardianDomain::Filesystem),
        RuleSeverity::Fixed(GuardianSeverity::Blocking),
        RuleConfidence::Fixed(GuardianConfidence::Confirmed),
        USER_OR_UNKNOWN_PROTECTION_ELIGIBILITY,
        [Block],
        "artifact_ownership_unsafe"
    ),
    rule!(
        PerformanceRulesInvalid,
        [PerformanceRulesInvalid],
        RuleDomain::Fixed(GuardianDomain::Performance),
        RuleSeverity::SupportingFactOr(GuardianSeverity::Degraded),
        RuleConfidence::SupportingFactOr(GuardianConfidence::High),
        PERFORMANCE_RECORD_ELIGIBILITY,
        [RecordOnly, Warn],
        "performance_rules_invalid"
    ),
    rule!(
        PerformanceHealthDegraded,
        [PerformanceHealthDegraded],
        RuleDomain::Fixed(GuardianDomain::Performance),
        RuleSeverity::SupportingFactOr(GuardianSeverity::Degraded),
        RuleConfidence::SupportingFactOr(GuardianConfidence::High),
        PERFORMANCE_RECORD_ELIGIBILITY,
        [RecordOnly, Warn],
        "performance_health_degraded"
    ),
    rule!(
        PerformanceHealthInvalid,
        [PerformanceHealthInvalid],
        RuleDomain::Fixed(GuardianDomain::Performance),
        RuleSeverity::SupportingFactOr(GuardianSeverity::Degraded),
        RuleConfidence::SupportingFactOr(GuardianConfidence::High),
        PERFORMANCE_RECORD_ELIGIBILITY,
        [RecordOnly, Warn],
        "performance_health_invalid"
    ),
    rule!(
        PerformanceFallbackSelected,
        [PerformanceFallbackSelected, PerformanceHealthFallback],
        RuleDomain::Fixed(GuardianDomain::Performance),
        RuleSeverity::SupportingFactOr(GuardianSeverity::Warning),
        RuleConfidence::SupportingFactOr(GuardianConfidence::High),
        PERFORMANCE_RECORD_ELIGIBILITY,
        [RecordOnly, Warn],
        "performance_fallback_selected"
    ),
    rule!(
        PerformanceRepeatedFailureMemory,
        [PerformanceRepeatedFailureMemory],
        RuleDomain::Fixed(GuardianDomain::Performance),
        RuleSeverity::SupportingFactOr(GuardianSeverity::Degraded),
        RuleConfidence::SupportingFactOr(GuardianConfidence::High),
        PERFORMANCE_MEMORY_ELIGIBILITY,
        [RecordOnly, Warn],
        "performance_repeated_failure_memory"
    ),
    rule!(
        PerformanceUserOwnedConflict,
        [PerformanceUserOwnedConflict],
        RuleDomain::Fixed(GuardianDomain::Performance),
        RuleSeverity::SupportingFactOr(GuardianSeverity::Blocking),
        RuleConfidence::SupportingFactOr(GuardianConfidence::Confirmed),
        PERFORMANCE_USER_CONFLICT_ELIGIBILITY,
        [RecordOnly, Warn, AskUser, Block],
        "performance_user_owned_conflict"
    ),
    rule!(
        ProcessLifecycleObserved,
        [
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
        RuleDomain::Fixed(GuardianDomain::Session),
        RuleSeverity::Fixed(GuardianSeverity::Info),
        RuleConfidence::Fixed(GuardianConfidence::High),
        RECORD_ONLY_ELIGIBILITY,
        [RecordOnly],
        "process_lifecycle_observed"
    ),
    rule!(
        PersistedStateSchemaInvalid,
        [PersistedStateSchemaInvalid],
        RuleDomain::Fixed(GuardianDomain::State),
        RuleSeverity::Fixed(GuardianSeverity::Warning),
        RuleConfidence::Fixed(GuardianConfidence::Confirmed),
        RECORD_ONLY_ELIGIBILITY,
        [Warn, RecordOnly],
        "persisted_state_schema_invalid"
    ),
];

pub(super) fn rule_for_diagnosis(id: DiagnosisId) -> Option<&'static DiagnosisRule> {
    DIAGNOSIS_RULES.iter().find(|rule| rule.id == id)
}
