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
    PlannedFile, install_and_record, managed_file_variants, uninstall, verified_removable_variants,
};
pub use manifest::{
    ContentManifest, EntrySource, ManifestEntry, ReconcileReport, UnidentifiedRecord,
    UnmanagedFile, entry_file_present, entry_path_matches, reconcile, sha512_file,
};
pub use model::{
    CanonicalContent, CanonicalId, ContentDependency, ContentDetail, ContentKind, ContentVersion,
    DependencyKind, FileRef, GalleryImage, ProjectMetadata, ProviderId, ProviderRef,
    ReleaseChannel, VersionIdentity,
};
pub use modrinth::ModrinthProvider;
pub use pack::{
    PackFile, PackFinalizeContext, PackIndex, PackInstallReport, PackLoader, install_pack,
    install_pack_files, install_pack_files_with_finalize, read_pack_index,
};
pub use provider::{ContentProvider, ContentQuery, LoaderGameFilter, Page, SortOrder};
pub use registry::ContentRegistry;
