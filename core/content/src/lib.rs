pub mod error;
pub mod install;
mod limits;
pub mod manifest;
mod managed_transaction;
pub mod model;
mod modrinth;
pub mod pack;
pub mod resolver;
mod transaction;

pub use error::{ContentError, ContentResult};
pub use install::{
    ManagedRemoval, ModFileDeleteOutcome, ModFileMutationError, ModFileToggleOutcome, PlannedFile,
    ProtectedManagedPaths, delete_local_mod_file, toggle_mod_file, verified_removable_variants,
};
pub use manifest::{
    ContentManifest, ManifestEntry, PendingManifestEntry, entry_file_present, entry_path_matches,
    sha512_file,
};
pub use managed_transaction::{
    LiveManagedContent, ManagedContentExecutionPlan, ManagedContentOperationProjection,
    ManagedContentPayloadSource,
    ObservedContentManifest, decode_observed_content_manifest, derive_live_managed_content,
    managed_content_liveness_paths, managed_install_observation_paths,
    managed_uninstall_observation_paths, missing_managed_content_observations,
    plan_managed_content_install, plan_managed_content_uninstall,
};
pub use model::{
    CanonicalContent, CanonicalId, ContentDependency, ContentDetail, ContentKind, ContentQuery,
    ContentVersion, DependencyKind, FileRef, GalleryImage, LoaderGameFilter,
    ManagedContentFileName, Page, ProjectMetadata, ProviderId, ReleaseChannel, SortOrder,
    VersionIdentity,
};
pub use modrinth::ContentService;
pub use pack::{
    ManagedPackAvailability, PackFile, PackFinalizeContext, PackIndex, PackInstallOptions,
    PackInstallReport, PackLoader, install_pack_files_with_finalize, read_pack_index,
};
pub use resolver::{
    ContentResolution, ResolutionConflict, ResolutionConflictKind, ResolutionConflictReason,
    ResolutionError, ResolutionLimitExceeded, ResolutionLimitKind, ResolutionReason,
    ResolutionSelection, ResolutionTarget, ResolvedContentItem,
    canonicalize_version_only_dependencies, has_unresolved_version_only_incompatibility,
    newer_version, pick_version, resolve_content, version_conflicts_with_installed,
    version_matches_filter,
};
