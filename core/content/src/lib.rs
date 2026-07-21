pub mod error;
pub mod install;
mod limits;
pub mod manifest;
pub mod model;
mod modrinth;
pub mod pack;
pub mod resolver;
mod transaction;

pub use error::{ContentError, ContentResult};
pub use install::{
    ManagedRemoval, ModFileDeleteOutcome, ModFileMutationError, ModFileToggleOutcome, PlannedFile,
    delete_local_mod_file, install_and_record, managed_file_variants, toggle_mod_file, uninstall,
    uninstall_many, verified_removable_variants,
};
pub use manifest::{
    ContentManifest, ManifestEntry, entry_file_present, entry_path_matches, sha512_file,
};
pub use model::{
    CanonicalContent, CanonicalId, ContentDependency, ContentDetail, ContentKind, ContentQuery,
    ContentVersion, DependencyKind, FileRef, GalleryImage, LoaderGameFilter, Page, ProjectMetadata,
    ProviderId, ReleaseChannel, SortOrder, VersionIdentity,
};
pub use modrinth::ContentService;
pub use pack::{
    PackFile, PackFinalizeContext, PackIndex, PackInstallOptions, PackInstallReport, PackLoader,
    install_pack_files_with_finalize, read_pack_index,
};
pub use resolver::{
    ContentResolution, ResolutionConflict, ResolutionConflictKind, ResolutionConflictReason,
    ResolutionError, ResolutionLimitExceeded, ResolutionLimitKind, ResolutionReason,
    ResolutionSelection, ResolutionTarget, ResolvedContentItem,
    canonicalize_version_only_dependencies, has_unresolved_version_only_incompatibility,
    newer_version, pick_version, resolve_content, version_conflicts_with_installed,
    version_matches_filter,
};
