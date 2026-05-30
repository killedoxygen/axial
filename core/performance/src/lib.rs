pub mod health;
pub mod install;
pub mod modrinth;
pub mod resolve;
pub mod rules;
pub mod rules_cache;
pub mod state;
pub mod status;
pub mod types;

pub use health::{BundleHealth, derive_health};
pub use install::{InstallError, PERFORMANCE_RULES_URL_ENV, PerformanceManager, RulesRefreshError};
pub use resolve::{
    ResolveError, builtin_manifest, detect_hardware, extract_base_version,
    infer_loader_from_version_id, parse_mode, resolve_plan,
};
pub use rules_cache::{
    LoadedRulesCache, RulesCacheSnapshot, RulesCacheState, RulesCacheStatus, rules_cache_path,
};
pub use state::{RollbackSnapshotSummary, StateError, load_state, remove_state, save_state};
pub use status::{
    FamilyCoverage, OwnershipClass, PerformanceRulesStatus, RuleChannel, RuleSource,
    RulesValidation, rules_status, rules_status_for,
};
pub use types::{
    CompositionPlan, CompositionState, CompositionTier, HardwareProfile, InstalledMod, Manifest,
    PerformanceMode, ResolutionRequest,
};
