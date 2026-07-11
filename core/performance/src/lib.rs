pub mod effective;
mod file_identity;
mod health;
pub mod install;
pub mod modrinth;
pub mod resolve;
pub mod rules;
pub mod rules_cache;
pub mod signature;
mod state;
pub mod status;
pub mod types;

pub(crate) const MANAGED_ARTIFACT_MAX_BYTES: u64 = 512 << 20;

pub use effective::{
    EffectiveContributionSource, EffectiveFallbackPlan, EffectiveInstrumentationMode,
    EffectiveInstrumentationPolicy, EffectiveJvmContribution, EffectiveLaunchSmoothing,
    EffectiveLaunchSmoothingPolicy, EffectiveLoaderPosture, EffectiveManagedArtifact,
    EffectivePerformanceComposition, EffectivePerformanceExplanation,
    EffectivePerformanceHealthRequirements, EffectivePerformancePlan, effective_performance_plan,
};
pub use health::BundleHealth;
pub use install::{
    InstallError, ManagedCompositionAuthority, ManagedCompositionInspection, ManagedIdentityError,
    ManagedIndeterminate, ManagedInstanceIdentity, ManagedMutationError, ManagedResolvedInspection,
    PERFORMANCE_RULES_URL_ENV, PerformanceManager, PerformanceRulesAuthority, RulesRefreshError,
    VerifiedRemoteRules, remote_rules_refresh_warning,
};
pub use resolve::{ResolveError, builtin_manifest, detect_hardware, parse_mode, resolve_plan};
pub use rules_cache::{
    RULES_CACHE_MAX_BYTES, RulesCacheSnapshot, RulesCacheState, RulesCacheStatus, rules_cache_path,
};
pub use signature::{
    PERFORMANCE_RULES_PUBLIC_KEY_ENV, RULES_KEY_ID_HEADER, RULES_SIGNATURE_HEADER,
    RemoteRulesVerifier, RulesSignatureError, RulesSignatureMetadata, canonical_manifest_payload,
};
pub use state::{RollbackSnapshotSummary, StateError};
pub use status::{
    FamilyCoverage, PerformanceRulesStatus, RuleChannel, RuleSource, RulesValidation, rules_status,
    rules_status_for,
};
pub use types::{
    CompositionPlan, CompositionState, CompositionTier, HardwareProfile, InstalledMod,
    ManagedArtifactIntegrity, ManagedArtifactProvider, ManagedArtifactSource, Manifest,
    ModCondition, OwnershipClass, PerformanceMode, ResolutionRequest,
};
