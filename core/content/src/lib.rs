pub mod error;
pub mod install;
pub mod manifest;
pub mod model;
pub mod modrinth;
pub mod pack;
pub mod provider;
pub mod registry;
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
    CanonicalContent, CanonicalId, ContentDependency, ContentDetail, ContentKind, ContentVersion,
    DependencyKind, FileRef, GalleryImage, ProjectMetadata, ProviderId, ProviderRef,
    ReleaseChannel, VersionIdentity,
};
pub use modrinth::ModrinthProvider;
pub use pack::{
    PackFile, PackFinalizeContext, PackIndex, PackInstallOptions, PackInstallReport, PackLoader,
    install_pack_files_with_finalize, read_pack_index,
};
pub use provider::{ContentProvider, ContentQuery, LoaderGameFilter, Page, SortOrder};
pub use registry::ContentRegistry;
