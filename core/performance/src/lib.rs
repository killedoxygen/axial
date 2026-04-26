pub mod health;
pub mod install;
pub mod modrinth;
pub mod resolve;
pub mod rules;
pub mod state;
pub mod types;

pub use health::{BundleHealth, derive_health};
pub use install::{InstallError, PerformanceManager};
pub use resolve::{
    ResolveError, builtin_manifest, detect_hardware, extract_base_version,
    infer_loader_from_version_id, parse_mode, resolve_plan,
};
pub use state::{StateError, load_state, remove_state, save_state};
pub use types::{
    CompositionPlan, CompositionState, CompositionTier, HardwareProfile, InstalledMod, Manifest,
    PerformanceMode, ResolutionRequest,
};
