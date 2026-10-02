//! Read-only predecessor inventory and immutable ordinary-instance import input.
//! The instance owner publishes independent payloads with its retained receipts.
//! Full profile cutover remains unavailable while feature conversions are pending.
//!
//! A serialized source path never grants authority. Composition admits sources
//! through the replacement root, without opening a root session on the old app.

pub(crate) mod history;
mod inventory;
mod metadata;
pub mod model;
mod prepare;
mod preview;
mod rules;
mod skins;

pub use inventory::{CaptureLimits, Inventory, ReadOnlySource};
pub use metadata::{
    METADATA_IMPORT_ARCHIVED_BENCHMARKS_MIGRATION, METADATA_IMPORT_ARCHIVED_CONTENT_MIGRATION,
    METADATA_IMPORT_ARCHIVED_OPERATIONS_MIGRATION, METADATA_IMPORT_ARCHIVED_REPORTS_MIGRATION,
    METADATA_IMPORT_HISTORY_MIGRATION, METADATA_IMPORT_IDENTITIES_MIGRATION,
    METADATA_IMPORT_MIGRATION, MetadataImportCommit, MetadataImportError, PreparedMetadataImport,
    metadata_install_history, metadata_status,
};
pub use model::{ImportBlocker, ImportPreview, LegacyInstance, RetainedObligation};
pub use prepare::PreparedInstanceImport;
pub use preview::ImportPreviews;
pub use rules::{PreparedRulesImport, RulesImportError, rules_status};
pub use skins::{PreparedSkinImport, SkinImportError, skin_status};

pub type ImportResult<T> = Result<T, ImportError>;

#[derive(Debug, thiserror::Error)]
pub enum ImportError {
    #[error("The predecessor data is malformed or unsupported.")]
    InvalidData,
    #[error("The predecessor data changed. Create a new import preview.")]
    SourceChanged,
    #[error("The import exceeds the configured file or byte limit.")]
    LimitExceeded,
    #[error("The import preview was cancelled.")]
    Cancelled,
    #[error("No predecessor profile has been admitted for preview.")]
    NoSource,
    #[error("The import preview is temporarily unavailable.")]
    Unavailable,
    #[error("The import could not read or preserve a file.")]
    Io(#[from] std::io::Error),
}

#[cfg(test)]
pub(crate) mod tests;
