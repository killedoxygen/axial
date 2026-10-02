use serde::{Deserialize, Serialize};
use serde_json::Value;
use ts_rs::TS;

/// Private retained evidence. Never export raw obligations on public routes:
/// legacy records can contain file projections and user-authored text.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RetainedObligation {
    pub source_record: String,
    pub instance_ids: Vec<String>,
    /// None means malformed data or an opaque file/directory/link. For captured
    /// regular files the exact bytes remain readable, never replaced by defaults.
    pub original: Option<Value>,
    pub blocker: ImportBlocker,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum ImportBlocker {
    CutoverNotImplemented,
    MissingRequiredRecord,
    UnsupportedSchema,
    MissingInstanceSource,
    UnsupportedLoader,
    PendingDeletion,
    UnsettledOperation,
    ManagedStateRequiresConversion,
    ContentProvenanceRequiresConversion,
    BrowserPreferencesRequired,
    RetainedPreferenceRequiresConversion,
    InstanceMetadataRequiresConversion,
    SavedSkinsRequireConversion,
    RetainedHistoryRequiresConversion,
    AccountRequiresConversion,
    UnknownRetainedRecord,
    UnsafeFile,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LegacyInstance {
    pub legacy_id: String,
    /// Complete predecessor instance metadata; the owning registry converts it.
    pub original: Value,
    pub blockers: Vec<ImportBlocker>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct InstancePreview {
    pub legacy_id: String,
    pub name: String,
    /// Source conversion is supported. Publication still checks destination
    /// compatibility and source identity; this is not full profile cutover.
    pub ordinary_import_available: bool,
    /// Admitted loader identity, or bounded source spelling when conversion fails.
    pub loader_key: String,
    pub blockers: Vec<ImportBlocker>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct ImportPreview {
    /// A preview is never a successful import or permission to launch its rows.
    pub cutover_available: bool,
    pub fingerprint: String,
    /// Source-bound key for read-only reconciliation of this metadata import.
    pub metadata_import_id: String,
    /// Uses the same conversion as metadata preparation; unrelated retained
    /// obligations remain visible and still prevent a full profile cutover.
    pub metadata_import_available: bool,
    /// Source-bound key for read-only reconciliation of this saved-skin batch.
    pub skin_import_id: String,
    /// Exact v3 index and captured PNG conversion, independent of other slices.
    pub skin_import_available: bool,
    pub rules_import_id: String,
    /// Captured cache/history has a supported shape. Destination signing trust
    /// and conflicts are checked only when this separate import is submitted.
    pub rules_import_available: bool,
    pub instances: Vec<InstancePreview>,
    pub file_count: usize,
    pub byte_count: u64,
    pub offline_account_count: usize,
    pub microsoft_reauthentication_count: usize,
    pub saved_skin_count: usize,
    pub retained_obligation_count: usize,
    pub retained_records: Vec<RetainedRecordPreview>,
    pub blockers: Vec<ImportBlocker>,
}

/// Selects only a row of a preview admitted by native composition.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct InstanceImportRequest {
    pub fingerprint: String,
    pub legacy_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct InstanceImportResponse {
    pub legacy_id: String,
    pub instance: crate::instances::model::Instance,
    /// Importing one instance does not attest a complete profile migration.
    pub cutover_available: bool,
}

/// Current completed import identities only, never inferred from names or
/// launch readiness. Missing entries have no current completed/live mapping.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct InstanceImportMappings {
    pub fingerprint: String,
    /// Same source-bound identity exposed by the admitted preview, whether or
    /// not its separate account/settings import has been completed.
    pub metadata_import_id: String,
    pub instance_id_mapping: std::collections::BTreeMap<String, String>,
    pub cutover_available: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct MetadataImportRequest {
    pub metadata_import_id: String,
    pub fingerprint: String,
    pub expected_settings_revision: u64,
    pub expected_account_selection_revision: u64,
}

/// Revisions identify the original atomic import, not later destination edits.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct MetadataImportReceipt {
    pub metadata_import_id: String,
    pub imported_offline_account_count: usize,
    pub imported_microsoft_account_count: usize,
    /// Every legacy identity maps to its destination identity. Receipts created
    /// before this mapping was retained return null, never an invented mapping.
    pub account_id_mapping: Option<std::collections::BTreeMap<String, String>>,
    /// Absent means history completion was not proved, including old receipts.
    /// Zero is a verified empty source-global install-history snapshot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub global_install_history_count: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archived_launch_report_count: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archived_benchmark_count: Option<usize>,
    pub settings_revision: u64,
    pub account_selection_revision: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct MetadataImportResponse {
    pub receipt: MetadataImportReceipt,
    pub already_imported: bool,
    pub cutover_available: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct MetadataImportStatus {
    pub receipt: Option<MetadataImportReceipt>,
    pub cutover_available: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct SkinImportRequest {
    pub skin_import_id: String,
    pub fingerprint: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct SkinImportResponse {
    pub receipt: crate::skins::library::SkinImportReceipt,
    pub already_imported: bool,
    pub cutover_available: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct SkinImportStatus {
    pub receipt: Option<crate::skins::library::SkinImportReceipt>,
    pub cutover_available: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct RulesImportRequest {
    pub rules_import_id: String,
    pub fingerprint: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct RulesImportResponse {
    pub receipt: crate::performance::rules::RulesImportReceipt,
    pub already_imported: bool,
    /// Exact persisted-cache equality, not current trust or launch readiness.
    pub stored_cache_matches_import: bool,
    pub cutover_available: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct RulesImportStatus {
    pub receipt: Option<crate::performance::rules::RulesImportReceipt>,
    pub stored_cache_matches_import: bool,
    pub cutover_available: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct RetainedRecordPreview {
    /// Content-bound identifier. No source paths or raw journal text cross HTTP.
    pub record_id: String,
    pub instance_ids: Vec<String>,
    pub blocker: ImportBlocker,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct FileManifest {
    pub relative: String,
    pub size: u64,
    pub sha256: String,
}

pub(crate) fn legacy_id(value: &str) -> bool {
    value.len() == 16
        && value
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}
