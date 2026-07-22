pub mod flags;
pub mod instances;
pub mod models;
pub mod paths;
mod root;
pub mod store;

pub use flags::{FEATURE_FLAGS, FeatureFlagDef, FlagStage, find_flag};
pub use instances::{
    EnrichedInstance, INSTANCE_LAYOUT_DIRS, INSTANCE_REGISTRY_MAX_BYTES,
    INSTANCE_REGISTRY_MAX_ENTRIES, INSTANCE_REGISTRY_SCHEMA_VERSION, Instance,
    InstanceRegistrySnapshot, InstanceStore, InstanceStoreError, InstanceStoreStartup,
    LaunchActionState, LaunchActionTone, LaunchPrimaryAction, SHARED_INSTANCE_FILES,
    derive_instance_art_seed, generate_instance_id, is_canonical_instance_id,
};
pub use models::{
    AppConfig, AppConfigValidationError, LAUNCH_AUTH_MODE_OFFLINE, LAUNCH_AUTH_MODE_ONLINE,
    USERNAME_MAX_LEN, USERNAME_MIN_LEN, validate_launch_auth_mode, validate_username,
};
pub use paths::{AppPaths, AppPathsError};
pub use root::{
    AppRootClearReceipt, AppRootResetAuthority, AppRootSession,
    ExistingLibraryDirectoryAdmission, PersistedStateDirectories,
};
pub use store::{
    CONFIG_MAX_BYTES, ConfigStartupLoad, ConfigStore, ConfigStoreError, StartupFileProvenance,
};
