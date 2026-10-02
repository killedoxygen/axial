//! One atomic account/settings conversion. The completed receipt is an
//! idempotency fact, not a journal or an attestation of full profile cutover.

use std::{
    collections::{BTreeMap, HashSet},
    sync::Arc,
};

use serde::Deserialize;
use sha2::{Digest, Sha256};

use super::{
    ImportError, ImportResult, Inventory,
    model::{
        MetadataImportReceipt, MetadataImportRequest, MetadataImportResponse, MetadataImportStatus,
    },
};
use crate::{
    accounts::{
        directory::AccountDirectory,
        model::{AccountError, AccountId, MicrosoftIdentityImport, OfflineIdentityImport},
    },
    install::history::{
        self as install_history, CompletionProof, HistoryError, HistoryPage,
        MAX_COMPLETION_PROOF_BYTES, PreparedImport as PreparedInstallImport,
    },
    launch::reports::{
        ArchivedReportCompletionProof, MAX_COMPLETION_PROOF_BYTES as MAX_ARCHIVED_PROOF_BYTES,
        PreparedReportImport, ReportError,
    },
    library::ApplicationRootPin,
    performance::benchmarks::{
        ArchivedBenchmarkCompletionProof, BenchmarkError, MAX_ARCHIVED_BENCHMARK_PROOF_BYTES,
        PreparedBenchmarkImport,
    },
    performance::mutation::{
        ArchivedOperationCompletionProof, MAX_ARCHIVED_OPERATION_PROOF_BYTES, OperationImportError,
        PreparedOperationImport,
    },
    settings::{
        ConfigLaunchAuthMode, PreparedSettingsImport, SettingsCommit, SettingsError, SettingsStore,
        prepare_legacy_import,
    },
    storage::{
        Migration,
        rusqlite::{Connection, OptionalExtension, params},
    },
    tasks::CancellationToken,
};

pub const METADATA_IMPORT_MIGRATION: Migration = Migration {
    id: "profile_metadata_imports.v1",
    sql: "CREATE TABLE profile_metadata_imports (
        source_id TEXT PRIMARY KEY NOT NULL,
        fingerprint TEXT NOT NULL CHECK(length(fingerprint) = 64),
        import_id TEXT NOT NULL UNIQUE CHECK(length(import_id) = 64),
        account_count INTEGER NOT NULL CHECK(account_count BETWEEN 1 AND 256),
        settings_revision INTEGER NOT NULL CHECK(settings_revision > 0),
        selection_revision INTEGER NOT NULL CHECK(selection_revision >= 0)
    );",
};

pub const METADATA_IMPORT_IDENTITIES_MIGRATION: Migration = Migration {
    id: "profile_metadata_imports.v2",
    sql: "ALTER TABLE profile_metadata_imports ADD COLUMN microsoft_account_count INTEGER NOT NULL DEFAULT 0
        CHECK(microsoft_account_count BETWEEN 0 AND account_count);
    ALTER TABLE profile_metadata_imports ADD COLUMN account_id_mapping TEXT
        CHECK(account_id_mapping IS NULL OR length(CAST(account_id_mapping AS BLOB)) <= 131072);",
};

pub const METADATA_IMPORT_HISTORY_MIGRATION: Migration = Migration {
    id: "profile_metadata_imports.v3",
    sql: "ALTER TABLE profile_metadata_imports ADD COLUMN global_install_history_proof TEXT
        CHECK(global_install_history_proof IS NULL OR length(CAST(global_install_history_proof AS BLOB)) <= 16384);",
};

pub const METADATA_IMPORT_ARCHIVED_REPORTS_MIGRATION: Migration = Migration {
    id: "profile_metadata_imports.v4",
    sql: "ALTER TABLE profile_metadata_imports ADD COLUMN archived_launch_report_proof TEXT
        CHECK(archived_launch_report_proof IS NULL OR length(CAST(archived_launch_report_proof AS BLOB)) <= 131072);",
};

pub const METADATA_IMPORT_ARCHIVED_BENCHMARKS_MIGRATION: Migration = Migration {
    id: "profile_metadata_imports.v5",
    sql: "ALTER TABLE profile_metadata_imports ADD COLUMN archived_benchmark_proof TEXT
        CHECK(archived_benchmark_proof IS NULL OR length(CAST(archived_benchmark_proof AS BLOB)) <= 131072);",
};

pub const METADATA_IMPORT_ARCHIVED_OPERATIONS_MIGRATION: Migration = Migration {
    id: "profile_metadata_imports.v6",
    sql: "ALTER TABLE profile_metadata_imports ADD COLUMN archived_performance_operation_proof TEXT
        CHECK(archived_performance_operation_proof IS NULL OR length(CAST(archived_performance_operation_proof AS BLOB)) <= 16384);",
};

pub const METADATA_IMPORT_ARCHIVED_CONTENT_MIGRATION: Migration = Migration {
    id: "profile_metadata_imports.v7",
    sql: "ALTER TABLE profile_metadata_imports ADD COLUMN archived_content_history_proof TEXT
        CHECK(archived_content_history_proof IS NULL OR length(CAST(archived_content_history_proof AS BLOB)) <= 16384);",
};

pub const METADATA_IMPORT_UNSELECTED_ACCOUNTS_MIGRATION: Migration = Migration {
    id: "profile_metadata_imports.v8",
    sql: "CREATE TABLE profile_metadata_imports_v8 (
        source_id TEXT PRIMARY KEY NOT NULL,
        fingerprint TEXT NOT NULL CHECK(length(fingerprint) = 64),
        import_id TEXT NOT NULL UNIQUE CHECK(length(import_id) = 64),
        account_count INTEGER NOT NULL CHECK(account_count BETWEEN 0 AND 256),
        settings_revision INTEGER NOT NULL CHECK(settings_revision > 0),
        selection_revision INTEGER NOT NULL CHECK(selection_revision >= 0),
        microsoft_account_count INTEGER NOT NULL DEFAULT 0 CHECK(microsoft_account_count BETWEEN 0 AND account_count),
        account_id_mapping TEXT CHECK(account_id_mapping IS NULL OR length(CAST(account_id_mapping AS BLOB)) <= 131072),
        global_install_history_proof TEXT CHECK(global_install_history_proof IS NULL OR length(CAST(global_install_history_proof AS BLOB)) <= 16384),
        archived_launch_report_proof TEXT CHECK(archived_launch_report_proof IS NULL OR length(CAST(archived_launch_report_proof AS BLOB)) <= 131072),
        archived_benchmark_proof TEXT CHECK(archived_benchmark_proof IS NULL OR length(CAST(archived_benchmark_proof AS BLOB)) <= 131072),
        archived_performance_operation_proof TEXT CHECK(archived_performance_operation_proof IS NULL OR length(CAST(archived_performance_operation_proof AS BLOB)) <= 16384),
        archived_content_history_proof TEXT CHECK(archived_content_history_proof IS NULL OR length(CAST(archived_content_history_proof AS BLOB)) <= 16384)
    );
    INSERT INTO profile_metadata_imports_v8 (
        source_id, fingerprint, import_id, account_count, settings_revision, selection_revision,
        microsoft_account_count, account_id_mapping, global_install_history_proof,
        archived_launch_report_proof, archived_benchmark_proof,
        archived_performance_operation_proof, archived_content_history_proof
    ) SELECT source_id, fingerprint, import_id, account_count, settings_revision, selection_revision,
        microsoft_account_count, account_id_mapping, global_install_history_proof,
        archived_launch_report_proof, archived_benchmark_proof,
        archived_performance_operation_proof, archived_content_history_proof
      FROM profile_metadata_imports;
    DROP TABLE profile_metadata_imports;
    ALTER TABLE profile_metadata_imports_v8 RENAME TO profile_metadata_imports;",
};

const MAX_MAPPING_BYTES: usize = 131072;

#[derive(Debug, thiserror::Error)]
pub enum MetadataImportError {
    #[error(transparent)]
    Source(#[from] ImportError),
    #[error(transparent)]
    Settings(#[from] SettingsError),
}

/// Only a source inventory can construct this value. Caller-authored records or
/// paths never authorize a metadata import, and forgetting a preview does not
/// discard the retained source authority of already accepted work.
#[derive(Clone)]
pub struct PreparedMetadataImport {
    inventory: Arc<Inventory>,
    source_id: String,
    import_id: String,
    accounts: PreparedAccounts,
    settings: PreparedSettingsImport,
    global_history: Option<(Arc<PreparedInstallImport>, CompletionProof)>,
    archived_content: Option<(Arc<PreparedInstallImport>, CompletionProof)>,
    archived_reports: Option<(Arc<PreparedReportImport>, ArchivedReportCompletionProof)>,
    archived_benchmarks: Option<(
        Arc<PreparedBenchmarkImport>,
        ArchivedBenchmarkCompletionProof,
    )>,
    archived_operations: Option<(
        Arc<PreparedOperationImport>,
        ArchivedOperationCompletionProof,
    )>,
}

/// Telemetry consent is published by the accepted command's owned consent
/// guard. On receipt replay, settings contains CURRENT persisted preferences
/// and replacement identity, never the original import's stale consent.
pub struct MetadataImportCommit {
    pub response: MetadataImportResponse,
    pub settings: SettingsCommit,
}

impl Inventory {
    pub(super) fn metadata_import_id(&self) -> ImportResult<String> {
        Ok(import_id(&self.source_identity()?, self.fingerprint()))
    }

    pub(super) fn metadata_import_available(&self) -> bool {
        self.metadata_inputs().is_ok()
    }

    pub(super) fn prepare_metadata(
        self: &Arc<Self>,
        fingerprint: &str,
    ) -> ImportResult<PreparedMetadataImport> {
        self.revalidate()?;
        if self.fingerprint() != fingerprint {
            return Err(ImportError::SourceChanged);
        }
        let (settings, accounts) = self.metadata_inputs()?;
        let source_id = self.source_identity()?;
        let global_history = match super::history::prepare_global_install_history(self) {
            Ok(batch) => {
                let proof = batch
                    .completion_proof(&source_id)
                    .map_err(|_| ImportError::InvalidData)?;
                Some((Arc::new(batch), proof))
            }
            Err(ImportError::InvalidData | ImportError::LimitExceeded) => None,
            Err(error) => return Err(error),
        };
        let archived_content = match super::history::prepare_archived_content_history(self) {
            Ok(batch) => {
                let combined_valid = global_history.as_ref().is_none_or(|(global, _)| {
                    let mut combined = global.as_ref().clone();
                    combined.append(&batch).is_ok()
                });
                if combined_valid {
                    let proof = batch
                        .archived_completion_proof(&source_id)
                        .map_err(|_| ImportError::InvalidData)?;
                    Some((Arc::new(batch), proof))
                } else {
                    None
                }
            }
            Err(ImportError::InvalidData | ImportError::LimitExceeded) => None,
            Err(error) => return Err(error),
        };
        let (archived_reports, archived_benchmarks) =
            match super::history::prepare_archived_history(self) {
                Ok(history) => {
                    let proof = history
                        .reports
                        .completion_proof(&source_id)
                        .map_err(|_| ImportError::InvalidData)?;
                    let benchmarks = history
                        .benchmarks
                        .map(|batch| {
                            let proof = batch
                                .completion_proof(&source_id)
                                .map_err(|_| ImportError::InvalidData)?;
                            Ok::<_, ImportError>((batch, proof))
                        })
                        .transpose()?;
                    (Some((history.reports, proof)), benchmarks)
                }
                Err(ImportError::InvalidData | ImportError::LimitExceeded) => (None, None),
                Err(error) => return Err(error),
            };
        let archived_operations = match super::history::prepare_archived_operations(self) {
            Ok(batch) => {
                let proof = batch
                    .completion_proof(&source_id)
                    .map_err(|_| ImportError::InvalidData)?;
                Some((Arc::new(batch), proof))
            }
            Err(ImportError::InvalidData | ImportError::LimitExceeded) => None,
            Err(error) => return Err(error),
        };
        self.revalidate()?;
        Ok(PreparedMetadataImport {
            inventory: Arc::clone(self),
            import_id: import_id(&source_id, fingerprint),
            source_id,
            accounts,
            settings,
            global_history,
            archived_content,
            archived_reports,
            archived_benchmarks,
            archived_operations,
        })
    }

    fn metadata_inputs(&self) -> ImportResult<(PreparedSettingsImport, PreparedAccounts)> {
        let settings = prepare_legacy_import(
            &serde_json::from_slice(&self.record_bytes("profile/config.json")?)
                .map_err(|_| ImportError::InvalidData)?,
        )
        .map_err(|_| ImportError::InvalidData)?;
        let accounts = prepare_accounts(self, &settings)?;
        Ok((settings, accounts))
    }
}

impl PreparedMetadataImport {
    /// Run in blocking accepted work while holding telemetry's consent guard.
    /// All filesystem validation precedes the short metadata-only transaction.
    pub fn commit(
        &self,
        settings: &SettingsStore,
        accounts: &AccountDirectory,
        destination: &ApplicationRootPin,
        request: &MetadataImportRequest,
        cancel: &CancellationToken,
    ) -> Result<MetadataImportCommit, MetadataImportError> {
        if !accounts.uses_metadata(settings.metadata()) {
            return Err(SettingsError::Unavailable.into());
        }
        if request.fingerprint != self.inventory.fingerprint()
            || request.metadata_import_id != self.import_id
        {
            return Err(ImportError::SourceChanged.into());
        }
        check_cancel(cancel)?;
        let root = destination.directory().map_err(ImportError::Io)?;
        self.inventory.validate_destination_root(&root)?;
        check_cancel(cancel)?;
        let result = settings.commit_prepared_import(
            &self.settings,
            request.expected_settings_revision,
            |transaction, config| {
                let existing = transaction.query_row(
                    "SELECT fingerprint, import_id FROM profile_metadata_imports WHERE source_id = ?1",
                    [&self.source_id],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
                ).optional()?;
                if let Some((fingerprint, id)) = existing {
                    if fingerprint != self.inventory.fingerprint() || id != self.import_id {
                        return Err(SettingsError::Conflict);
                    }
                    let mut stored = read_receipt(transaction, &id)?.ok_or(SettingsError::Corrupt)?;
                    if let Some((batch, proof)) = &self.global_history {
                        match &stored.global_history {
                            Some(existing) if existing != proof => return Err(SettingsError::Conflict),
                            Some(_) => {}
                            None => {
                                if cancel.is_cancelled() {
                                    return Err(SettingsError::Unavailable);
                                }
                                batch.insert_in(transaction).map_err(history_error)?;
                                if transaction.execute(
                                    "UPDATE profile_metadata_imports SET global_install_history_proof=?1
                                     WHERE source_id=?2 AND fingerprint=?3 AND import_id=?4
                                     AND global_install_history_proof IS NULL",
                                    params![encode_proof(proof)?, self.source_id, fingerprint, id],
                                )? != 1 {
                                    return Err(SettingsError::Conflict);
                                }
                                stored.global_history = Some(proof.clone());
                                stored.receipt.global_install_history_count = Some(proof.count());
                                if cancel.is_cancelled() {
                                    return Err(SettingsError::Unavailable);
                                }
                            }
                        }
                    }
                    if let Some((batch, proof)) = &self.archived_content {
                        match &stored.archived_content {
                            Some(existing) if existing != proof => return Err(SettingsError::Conflict),
                            Some(_) => {}
                            None => {
                                if cancel.is_cancelled() { return Err(SettingsError::Unavailable); }
                                batch.insert_in(transaction).map_err(history_error)?;
                                if transaction.execute(
                                    "UPDATE profile_metadata_imports SET archived_content_history_proof=?1
                                     WHERE source_id=?2 AND fingerprint=?3 AND import_id=?4
                                     AND archived_content_history_proof IS NULL",
                                    params![encode_proof(proof)?, self.source_id, fingerprint, id],
                                )? != 1 {
                                    return Err(SettingsError::Conflict);
                                }
                                stored.archived_content = Some(proof.clone());
                                stored.receipt.archived_content_operation_count = Some(proof.count());
                                if cancel.is_cancelled() { return Err(SettingsError::Unavailable); }
                            }
                        }
                    }
                    if let Some((batch, proof)) = &self.archived_reports {
                        match &stored.archived_reports {
                            Some(existing) if existing != proof => return Err(SettingsError::Conflict),
                            Some(_) => {}
                            None => {
                                if cancel.is_cancelled() { return Err(SettingsError::Unavailable); }
                                batch.insert_in(transaction).map_err(report_error)?;
                                if transaction.execute(
                                    "UPDATE profile_metadata_imports SET archived_launch_report_proof=?1
                                     WHERE source_id=?2 AND fingerprint=?3 AND import_id=?4
                                     AND archived_launch_report_proof IS NULL",
                                    params![encode_archived_proof(proof)?, self.source_id, fingerprint, id],
                                )? != 1 {
                                    return Err(SettingsError::Conflict);
                                }
                                stored.archived_reports = Some(proof.clone());
                                stored.receipt.archived_launch_report_count = Some(proof.count());
                                if cancel.is_cancelled() { return Err(SettingsError::Unavailable); }
                            }
                        }
                    }
                    if let Some((batch, proof)) = &self.archived_benchmarks {
                        match &stored.archived_benchmarks {
                            Some(existing) if existing != proof => return Err(SettingsError::Conflict),
                            Some(_) => {}
                            None => {
                                if cancel.is_cancelled() { return Err(SettingsError::Unavailable); }
                                batch.insert_in(transaction).map_err(benchmark_error)?;
                                if transaction.execute(
                                    "UPDATE profile_metadata_imports SET archived_benchmark_proof=?1
                                     WHERE source_id=?2 AND fingerprint=?3 AND import_id=?4
                                     AND archived_benchmark_proof IS NULL",
                                    params![encode_benchmark_proof(proof)?, self.source_id, fingerprint, id],
                                )? != 1 {
                                    return Err(SettingsError::Conflict);
                                }
                                stored.archived_benchmarks = Some(proof.clone());
                                stored.receipt.archived_benchmark_count = Some(proof.count());
                                if cancel.is_cancelled() { return Err(SettingsError::Unavailable); }
                            }
                        }
                    }
                    if let Some((batch, proof)) = &self.archived_operations {
                        match &stored.archived_operations {
                            Some(existing) if existing != proof => return Err(SettingsError::Conflict),
                            Some(_) => {}
                            None => {
                                if cancel.is_cancelled() { return Err(SettingsError::Unavailable); }
                                batch.insert_in(transaction).map_err(operation_error)?;
                                if transaction.execute(
                                    "UPDATE profile_metadata_imports SET archived_performance_operation_proof=?1
                                     WHERE source_id=?2 AND fingerprint=?3 AND import_id=?4
                                     AND archived_performance_operation_proof IS NULL",
                                    params![encode_operation_proof(proof)?, self.source_id, fingerprint, id],
                                )? != 1 {
                                    return Err(SettingsError::Conflict);
                                }
                                stored.archived_operations = Some(proof.clone());
                                stored.receipt.archived_performance_operation_count = Some(proof.count());
                                if cancel.is_cancelled() { return Err(SettingsError::Unavailable); }
                            }
                        }
                    }
                    return Ok((false, stored));
                }
                if cancel.is_cancelled() {
                    return Err(SettingsError::Unavailable);
                }
                let snapshot = AccountDirectory::import_identities_in_transaction(
                    transaction,
                    &self.accounts.offline,
                    &self.accounts.microsoft,
                    self.accounts.selection.as_ref().map(|(id, _, _)| id.as_str()),
                    request.expected_account_selection_revision,
                ).map_err(account_error)?;
                config.account_selection_revision = snapshot.selection_revision;
                if let Some((batch, _)) = &self.global_history {
                    batch.insert_in(transaction).map_err(history_error)?;
                }
                if let Some((batch, _)) = &self.archived_content {
                    batch.insert_in(transaction).map_err(history_error)?;
                }
                if let Some((batch, _)) = &self.archived_reports {
                    batch.insert_in(transaction).map_err(report_error)?;
                }
                if let Some((batch, _)) = &self.archived_benchmarks {
                    batch.insert_in(transaction).map_err(benchmark_error)?;
                }
                if let Some((batch, _)) = &self.archived_operations {
                    batch.insert_in(transaction).map_err(operation_error)?;
                }
                let completed = MetadataImportReceipt {
                    metadata_import_id: self.import_id.clone(),
                    imported_offline_account_count: self.accounts.offline.len(),
                    imported_microsoft_account_count: self.accounts.microsoft.len(),
                    account_id_mapping: Some(self.accounts.mapping.clone()),
                    global_install_history_count: self.global_history.as_ref().map(|(_, proof)| proof.count()),
                    archived_content_operation_count: self.archived_content.as_ref().map(|(_, proof)| proof.count()),
                    archived_launch_report_count: self.archived_reports.as_ref().map(|(_, proof)| proof.count()),
                    archived_benchmark_count: self.archived_benchmarks.as_ref().map(|(_, proof)| proof.count()),
                    archived_performance_operation_count: self.archived_operations.as_ref().map(|(_, proof)| proof.count()),
                    settings_revision: request.expected_settings_revision.checked_add(1)
                        .ok_or(SettingsError::Unavailable)?,
                    account_selection_revision: snapshot.selection_revision,
                };
                if transaction.execute(
                    "INSERT INTO profile_metadata_imports(source_id, fingerprint, import_id, account_count, settings_revision, selection_revision, microsoft_account_count, account_id_mapping, global_install_history_proof, archived_launch_report_proof, archived_benchmark_proof, archived_performance_operation_proof, archived_content_history_proof) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
                    params![self.source_id, self.inventory.fingerprint(), self.import_id,
                        self.accounts.mapping.len(), completed.settings_revision,
                        completed.account_selection_revision, completed.imported_microsoft_account_count,
                        encode_mapping(&self.accounts.mapping)?,
                        self.global_history.as_ref().map(|(_, proof)| encode_proof(proof)).transpose()?,
                        self.archived_reports.as_ref().map(|(_, proof)| encode_archived_proof(proof)).transpose()?,
                        self.archived_benchmarks.as_ref().map(|(_, proof)| encode_benchmark_proof(proof)).transpose()?,
                        self.archived_operations.as_ref().map(|(_, proof)| encode_operation_proof(proof)).transpose()?,
                        self.archived_content.as_ref().map(|(_, proof)| encode_proof(proof)).transpose()?],
                )? != 1 {
                    return Err(SettingsError::Conflict);
                }
                if cancel.is_cancelled() {
                    return Err(SettingsError::Unavailable);
                }
                Ok((true, StoredReceipt {
                    receipt: completed,
                    source_id: self.source_id.clone(),
                    fingerprint: self.inventory.fingerprint().to_owned(),
                    global_history: self.global_history.as_ref().map(|(_, proof)| proof.clone()),
                    archived_content: self.archived_content.as_ref().map(|(_, proof)| proof.clone()),
                    archived_reports: self.archived_reports.as_ref().map(|(_, proof)| proof.clone()),
                    archived_benchmarks: self.archived_benchmarks.as_ref().map(|(_, proof)| proof.clone()),
                    archived_operations: self.archived_operations.as_ref().map(|(_, proof)| proof.clone()),
                }))
            },
            |transaction, receipt| verify_receipt(transaction, receipt),
        );
        // Cancellation may win before persistence, never after a successful
        // commit. Returning cancellation after commit would hide its outcome.
        let (settings, imported, receipt) = match result {
            Err(SettingsError::Unavailable) if cancel.is_cancelled() => {
                return Err(ImportError::Cancelled.into());
            }
            result => result?,
        };
        Ok(MetadataImportCommit {
            response: MetadataImportResponse {
                receipt: receipt.receipt,
                already_imported: !imported,
                cutover_available: false,
            },
            settings,
        })
    }
}

/// A source-independent read reconciles an unknown HTTP outcome, even if the
/// preview was forgotten or the old volume is no longer connected.
pub fn metadata_status(
    settings: &SettingsStore,
    import_id: &str,
) -> Result<MetadataImportStatus, MetadataImportError> {
    if !valid_identity(import_id) {
        return Err(ImportError::InvalidData.into());
    }
    let receipt = settings.metadata().read(|connection| {
        let transaction = connection.unchecked_transaction()?;
        let stored = read_receipt(&transaction, import_id)?;
        if let Some(stored) = &stored {
            stored.verify_history(&transaction)?;
        }
        transaction.commit()?;
        Ok::<_, SettingsError>(stored.map(|stored| stored.receipt))
    })?;
    Ok(MetadataImportStatus {
        receipt,
        cutover_available: false,
    })
}

/// The receipt, not a current instance or a caller-supplied source ID, selects
/// the immutable history snapshot. A missing proof is not a verified empty page.
pub fn metadata_install_history(
    settings: &SettingsStore,
    id: &str,
    after: Option<&str>,
) -> Result<HistoryPage, MetadataImportError> {
    if !valid_identity(id) || install_history::validate_cursor(after).is_err() {
        return Err(ImportError::InvalidData.into());
    }
    let page = settings.metadata().read(|connection| {
        let transaction = connection.unchecked_transaction()?;
        let stored = read_receipt(&transaction, id)?.ok_or(SettingsError::Unavailable)?;
        if stored.global_history.is_none() && stored.archived_content.is_none() {
            return Err(SettingsError::Unavailable);
        }
        let page = install_history::read_completed_in(
            &transaction,
            &stored.source_id,
            stored.global_history.as_ref(),
            stored.archived_content.as_ref(),
            after,
        )
        .map_err(history_error)?;
        transaction.commit()?;
        Ok::<_, SettingsError>(page)
    })?;
    Ok(page)
}

#[derive(Debug, PartialEq, Eq)]
struct StoredReceipt {
    receipt: MetadataImportReceipt,
    source_id: String,
    fingerprint: String,
    global_history: Option<CompletionProof>,
    archived_content: Option<CompletionProof>,
    archived_reports: Option<ArchivedReportCompletionProof>,
    archived_benchmarks: Option<ArchivedBenchmarkCompletionProof>,
    archived_operations: Option<ArchivedOperationCompletionProof>,
}

impl StoredReceipt {
    fn verify_history(&self, connection: &Connection) -> Result<(), SettingsError> {
        if self.global_history.is_some() || self.archived_content.is_some() {
            install_history::read_completed_in(
                connection,
                &self.source_id,
                self.global_history.as_ref(),
                self.archived_content.as_ref(),
                None,
            )
            .map_err(history_error)?;
        }
        if let Some(proof) = &self.archived_reports {
            proof
                .verify_in(connection, &self.source_id)
                .map_err(report_error)?;
        }
        if let Some(proof) = &self.archived_benchmarks {
            if self.archived_reports.is_none() {
                return Err(SettingsError::Corrupt);
            }
            proof
                .verify_in(connection, &self.source_id)
                .map_err(benchmark_error)?;
        }
        if let Some(proof) = &self.archived_operations {
            proof
                .verify_in(connection, &self.source_id)
                .map_err(operation_error)?;
        }
        Ok(())
    }
}

fn verify_receipt(connection: &Connection, expected: &StoredReceipt) -> Result<(), SettingsError> {
    let stored = read_receipt(connection, &expected.receipt.metadata_import_id)?
        .ok_or(SettingsError::Conflict)?;
    if &stored != expected {
        return Err(SettingsError::Conflict);
    }
    stored.verify_history(connection)
}

fn read_receipt(
    connection: &Connection,
    import_id: &str,
) -> Result<Option<StoredReceipt>, SettingsError> {
    let row = connection
        .query_row(
            "SELECT account_count, settings_revision, selection_revision, microsoft_account_count,
            length(CAST(account_id_mapping AS BLOB)),
            CASE WHEN length(CAST(account_id_mapping AS BLOB)) <= ?2 THEN account_id_mapping END,
            CASE WHEN length(CAST(source_id AS BLOB)) <= 64 THEN source_id END,
            fingerprint, length(CAST(global_install_history_proof AS BLOB)),
            CASE WHEN length(CAST(global_install_history_proof AS BLOB)) <= ?3 THEN global_install_history_proof END,
            length(CAST(archived_launch_report_proof AS BLOB)),
            CASE WHEN length(CAST(archived_launch_report_proof AS BLOB)) <= ?4 THEN archived_launch_report_proof END,
            length(CAST(archived_benchmark_proof AS BLOB)),
            CASE WHEN length(CAST(archived_benchmark_proof AS BLOB)) <= ?5 THEN archived_benchmark_proof END,
            length(CAST(archived_performance_operation_proof AS BLOB)),
            CASE WHEN length(CAST(archived_performance_operation_proof AS BLOB)) <= ?6 THEN archived_performance_operation_proof END,
            length(CAST(archived_content_history_proof AS BLOB)),
            CASE WHEN length(CAST(archived_content_history_proof AS BLOB)) <= ?3 THEN archived_content_history_proof END
         FROM profile_metadata_imports WHERE import_id = ?1",
            params![import_id, MAX_MAPPING_BYTES, MAX_COMPLETION_PROOF_BYTES, MAX_ARCHIVED_PROOF_BYTES, MAX_ARCHIVED_BENCHMARK_PROOF_BYTES, MAX_ARCHIVED_OPERATION_PROOF_BYTES],
            |row| {
                Ok((
                    row.get::<_, usize>(0)?,
                    row.get::<_, u64>(1)?,
                    row.get::<_, u64>(2)?,
                    row.get::<_, usize>(3)?,
                    row.get::<_, Option<usize>>(4)?,
                    row.get::<_, Option<String>>(5)?,
                    row.get::<_, Option<String>>(6)?,
                    row.get::<_, String>(7)?,
                    row.get::<_, Option<usize>>(8)?,
                    row.get::<_, Option<String>>(9)?,
                    row.get::<_, Option<usize>>(10)?,
                    row.get::<_, Option<String>>(11)?,
                    row.get::<_, Option<usize>>(12)?,
                    row.get::<_, Option<String>>(13)?,
                    row.get::<_, Option<usize>>(14)?,
                    row.get::<_, Option<String>>(15)?,
                    row.get::<_, Option<usize>>(16)?,
                    row.get::<_, Option<String>>(17)?,
                ))
            },
        )
        .optional()?;
    let Some((
        count,
        settings_revision,
        account_selection_revision,
        microsoft_count,
        length,
        encoded,
        source_id,
        fingerprint,
        proof_length,
        encoded_proof,
        archived_length,
        encoded_archived,
        benchmark_length,
        encoded_benchmarks,
        operation_length,
        encoded_operations,
        content_length,
        encoded_content,
    )) = row
    else {
        return Ok(None);
    };
    if count > 256
        || microsoft_count > count
        || settings_revision == 0
        || settings_revision > 9_007_199_254_740_991
        || account_selection_revision > 9_007_199_254_740_991
        || length.is_some_and(|length| length > MAX_MAPPING_BYTES)
    {
        return Err(SettingsError::Corrupt);
    }
    let mapping = match (length, encoded) {
        (None, None) if microsoft_count == 0 && count > 0 => None,
        (Some(_), Some(encoded)) => Some(decode_mapping(&encoded, count, microsoft_count)?),
        _ => return Err(SettingsError::Corrupt),
    };
    let source_id = source_id.ok_or(SettingsError::Corrupt)?;
    if (proof_length.is_some()
        || archived_length.is_some()
        || benchmark_length.is_some()
        || operation_length.is_some()
        || content_length.is_some())
        && (!valid_identity(&source_id)
            || !valid_identity(&fingerprint)
            || self::import_id(&source_id, &fingerprint) != import_id)
    {
        return Err(SettingsError::Corrupt);
    }
    let global_history: Option<CompletionProof> = match (proof_length, encoded_proof) {
        (None, None) => None,
        (Some(length), Some(encoded)) if length <= MAX_COMPLETION_PROOF_BYTES => {
            Some(serde_json::from_str(&encoded).map_err(|_| SettingsError::Corrupt)?)
        }
        _ => return Err(SettingsError::Corrupt),
    };
    let archived_content: Option<CompletionProof> = match (content_length, encoded_content) {
        (None, None) => None,
        (Some(length), Some(encoded)) if length <= MAX_COMPLETION_PROOF_BYTES => {
            Some(serde_json::from_str(&encoded).map_err(|_| SettingsError::Corrupt)?)
        }
        _ => return Err(SettingsError::Corrupt),
    };
    let archived_reports: Option<ArchivedReportCompletionProof> =
        match (archived_length, encoded_archived) {
            (None, None) => None,
            (Some(length), Some(encoded)) if length <= MAX_ARCHIVED_PROOF_BYTES => {
                Some(serde_json::from_str(&encoded).map_err(|_| SettingsError::Corrupt)?)
            }
            _ => return Err(SettingsError::Corrupt),
        };
    let archived_benchmarks: Option<ArchivedBenchmarkCompletionProof> =
        match (benchmark_length, encoded_benchmarks) {
            (None, None) => None,
            (Some(length), Some(encoded)) if length <= MAX_ARCHIVED_BENCHMARK_PROOF_BYTES => {
                Some(serde_json::from_str(&encoded).map_err(|_| SettingsError::Corrupt)?)
            }
            _ => return Err(SettingsError::Corrupt),
        };
    let archived_operations: Option<ArchivedOperationCompletionProof> =
        match (operation_length, encoded_operations) {
            (None, None) => None,
            (Some(length), Some(encoded)) if length <= MAX_ARCHIVED_OPERATION_PROOF_BYTES => {
                Some(serde_json::from_str(&encoded).map_err(|_| SettingsError::Corrupt)?)
            }
            _ => return Err(SettingsError::Corrupt),
        };
    Ok(Some(StoredReceipt {
        receipt: MetadataImportReceipt {
            metadata_import_id: import_id.to_owned(),
            imported_offline_account_count: count - microsoft_count,
            imported_microsoft_account_count: microsoft_count,
            account_id_mapping: mapping,
            global_install_history_count: global_history.as_ref().map(CompletionProof::count),
            archived_content_operation_count: archived_content.as_ref().map(CompletionProof::count),
            archived_launch_report_count: archived_reports
                .as_ref()
                .map(ArchivedReportCompletionProof::count),
            archived_benchmark_count: archived_benchmarks
                .as_ref()
                .map(ArchivedBenchmarkCompletionProof::count),
            archived_performance_operation_count: archived_operations
                .as_ref()
                .map(ArchivedOperationCompletionProof::count),
            settings_revision,
            account_selection_revision,
        },
        source_id,
        fingerprint,
        global_history,
        archived_content,
        archived_reports,
        archived_benchmarks,
        archived_operations,
    }))
}

fn valid_identity(id: &str) -> bool {
    id.len() == 64
        && id
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn encode_proof(proof: &CompletionProof) -> Result<String, SettingsError> {
    let encoded = serde_json::to_string(proof).map_err(|_| SettingsError::Unavailable)?;
    if encoded.len() > MAX_COMPLETION_PROOF_BYTES {
        return Err(SettingsError::Unavailable);
    }
    Ok(encoded)
}

fn history_error(error: HistoryError) -> SettingsError {
    match error {
        HistoryError::Invalid => SettingsError::Corrupt,
        HistoryError::Conflict => SettingsError::Conflict,
        HistoryError::Storage(error) => SettingsError::Storage(error),
    }
}

fn encode_archived_proof(proof: &ArchivedReportCompletionProof) -> Result<String, SettingsError> {
    let encoded = serde_json::to_string(proof).map_err(|_| SettingsError::Unavailable)?;
    if encoded.len() > MAX_ARCHIVED_PROOF_BYTES {
        return Err(SettingsError::Unavailable);
    }
    Ok(encoded)
}

fn report_error(error: ReportError) -> SettingsError {
    match error {
        ReportError::Invalid | ReportError::TooLarge => SettingsError::Corrupt,
        ReportError::ConflictingSession => SettingsError::Conflict,
        ReportError::Storage(error) => SettingsError::Storage(error),
    }
}

fn encode_benchmark_proof(
    proof: &ArchivedBenchmarkCompletionProof,
) -> Result<String, SettingsError> {
    let encoded = serde_json::to_string(proof).map_err(|_| SettingsError::Unavailable)?;
    if encoded.len() > MAX_ARCHIVED_BENCHMARK_PROOF_BYTES {
        return Err(SettingsError::Unavailable);
    }
    Ok(encoded)
}

fn benchmark_error(error: BenchmarkError) -> SettingsError {
    match error {
        BenchmarkError::Storage(error) => SettingsError::Storage(error),
        BenchmarkError::ConflictingHistory => SettingsError::Conflict,
        _ => SettingsError::Corrupt,
    }
}

fn encode_operation_proof(
    proof: &ArchivedOperationCompletionProof,
) -> Result<String, SettingsError> {
    let encoded = serde_json::to_string(proof).map_err(|_| SettingsError::Unavailable)?;
    if encoded.len() > MAX_ARCHIVED_OPERATION_PROOF_BYTES {
        return Err(SettingsError::Unavailable);
    }
    Ok(encoded)
}

fn operation_error(error: OperationImportError) -> SettingsError {
    match error {
        OperationImportError::Invalid => SettingsError::Corrupt,
        OperationImportError::Conflict => SettingsError::Conflict,
        OperationImportError::Storage(error) => SettingsError::Storage(error),
    }
}

fn encode_mapping(mapping: &BTreeMap<String, String>) -> Result<String, SettingsError> {
    // Persist pairs so decoding rejects duplicate keys rather than silently
    // accepting JSON object key replacement. Public projection remains a map.
    let encoded = serde_json::to_string(&mapping.iter().collect::<Vec<_>>())
        .map_err(|_| SettingsError::Unavailable)?;
    if encoded.len() > MAX_MAPPING_BYTES {
        return Err(SettingsError::Unavailable);
    }
    Ok(encoded)
}

fn decode_mapping(
    encoded: &str,
    count: usize,
    microsoft_count: usize,
) -> Result<BTreeMap<String, String>, SettingsError> {
    let pairs: Vec<(String, String)> =
        serde_json::from_str(encoded).map_err(|_| SettingsError::Corrupt)?;
    if pairs.len() != count {
        return Err(SettingsError::Corrupt);
    }
    let mut mapping = BTreeMap::new();
    let mut destinations = HashSet::new();
    let mut microsoft = 0;
    for (source, destination) in pairs {
        if source.starts_with("offline-") {
            if source != destination || AccountId::parse(&source).is_err() {
                return Err(SettingsError::Corrupt);
            }
        } else {
            let parsed = uuid::Uuid::parse_str(&destination).map_err(|_| SettingsError::Corrupt)?;
            if !legacy_microsoft_id(&source)
                || parsed.is_nil()
                || parsed.hyphenated().to_string() != destination
            {
                return Err(SettingsError::Corrupt);
            }
            microsoft += 1;
        }
        if !destinations.insert(destination.clone())
            || mapping.insert(source, destination).is_some()
        {
            return Err(SettingsError::Corrupt);
        }
    }
    if microsoft != microsoft_count {
        return Err(SettingsError::Corrupt);
    }
    Ok(mapping)
}

fn import_id(source_id: &str, fingerprint: &str) -> String {
    let mut hash = Sha256::new();
    hash.update(b"axial-profile-metadata-import-v1\0");
    hash.update(source_id.as_bytes());
    hash.update([0]);
    hash.update(fingerprint.as_bytes());
    hex::encode(hash.finalize())
}

fn check_cancel(cancel: &CancellationToken) -> ImportResult<()> {
    if cancel.is_cancelled() {
        Err(ImportError::Cancelled)
    } else {
        Ok(())
    }
}

fn account_error(error: AccountError) -> SettingsError {
    match error {
        AccountError::AlreadyExists | AccountError::StaleCapture => SettingsError::Conflict,
        AccountError::InvalidStoredData => SettingsError::Corrupt,
        AccountError::Storage => SettingsError::Unavailable,
        _ => SettingsError::Validation("The imported account selection is invalid."),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyAccounts {
    schema: String,
    schema_version: u32,
    active_account_id: Option<String>,
    accounts: Vec<LegacyAccount>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyAccount {
    account_id: String,
    kind: String,
    display_name: String,
    login_id: Option<String>,
    minecraft_profile_id: Option<String>,
    offline_uuid: Option<String>,
    created_at: String,
    updated_at: String,
}

#[derive(Clone)]
pub(super) struct PreparedAccounts {
    pub(super) offline: Vec<OfflineIdentityImport>,
    pub(super) microsoft: Vec<MicrosoftIdentityImport>,
    mapping: BTreeMap<String, String>,
    selection: Option<(String, String, ConfigLaunchAuthMode)>,
}

pub(super) fn prepare_accounts(
    inventory: &Inventory,
    settings: &PreparedSettingsImport,
) -> ImportResult<PreparedAccounts> {
    let prepared = convert_accounts(
        serde_json::from_slice(&inventory.record_bytes("profile/accounts.json")?)
            .map_err(|_| ImportError::InvalidData)?,
    )?;
    let consistent = match &prepared.selection {
        Some((_, name, mode)) => {
            *mode == settings.config.launch_auth_mode && *name == settings.config.username
        }
        None => settings.config.launch_auth_mode == ConfigLaunchAuthMode::Offline,
    };
    if !consistent {
        return Err(ImportError::InvalidData);
    }
    Ok(prepared)
}

/// Shared by preview eligibility and both metadata and ordinary-payload import.
/// The predecessor stores login-derived IDs; only profile UUIDs become new
/// Microsoft identities. Login IDs are validated but never transferred.
pub(super) fn convert_accounts(value: serde_json::Value) -> ImportResult<PreparedAccounts> {
    let source: LegacyAccounts =
        serde_json::from_value(value).map_err(|_| ImportError::InvalidData)?;
    if source.schema != "axial.accounts"
        || source.schema_version != 1
        || source.accounts.len() > 256
    {
        return Err(ImportError::InvalidData);
    }
    let active = source.active_account_id;
    let mut offline = Vec::new();
    let mut microsoft = Vec::new();
    let mut mapping = BTreeMap::new();
    let mut destinations = HashSet::new();
    let mut selected = None;
    for account in source.accounts {
        let legacy_id = account.account_id.clone();
        let name = account.display_name.clone();
        let input = convert_account(account)?;
        let destination_id = input.destination_id()?;
        let mode = match input {
            ImportedIdentity::Offline(input) => {
                offline.push(input);
                ConfigLaunchAuthMode::Offline
            }
            ImportedIdentity::Microsoft(input) => {
                microsoft.push(input);
                ConfigLaunchAuthMode::Online
            }
        };
        if !destinations.insert(destination_id.clone())
            || mapping
                .insert(legacy_id.clone(), destination_id.clone())
                .is_some()
        {
            return Err(ImportError::InvalidData);
        }
        if active.as_ref() == Some(&legacy_id) {
            selected = Some((destination_id, name, mode));
        }
    }
    if active.is_some() && selected.is_none() {
        return Err(ImportError::InvalidData);
    }
    Ok(PreparedAccounts {
        offline,
        microsoft,
        mapping,
        selection: selected,
    })
}

fn legacy_microsoft_id(value: &str) -> bool {
    value
        .strip_prefix("microsoft-")
        .is_some_and(legacy_login_id)
}

fn legacy_login_id(value: &str) -> bool {
    let Some((nanos, sequence)) = value
        .strip_prefix("msa-")
        .and_then(|value| value.split_once('-'))
    else {
        return false;
    };
    let hex = |value: &str, max| {
        !value.is_empty()
            && value.len() <= max
            && value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    };
    hex(nanos, 32) && hex(sequence, 16)
}

enum ImportedIdentity {
    Offline(OfflineIdentityImport),
    Microsoft(MicrosoftIdentityImport),
}

impl ImportedIdentity {
    fn destination_id(&self) -> ImportResult<String> {
        match self {
            Self::Offline(input) => Ok(input.account_id.clone()),
            Self::Microsoft(input) => input.account_id().map_err(|_| ImportError::InvalidData),
        }
    }
}

pub(super) fn destination_account_id(value: &serde_json::Value) -> ImportResult<String> {
    convert_account(serde_json::from_value(value.clone()).map_err(|_| ImportError::InvalidData)?)?
        .destination_id()
}

fn convert_account(account: LegacyAccount) -> ImportResult<ImportedIdentity> {
    match account.kind.as_str() {
        "offline" => {
            if account.login_id.is_some() || account.minecraft_profile_id.is_some() {
                return Err(ImportError::InvalidData);
            }
            let input = OfflineIdentityImport {
                account_id: account.account_id,
                display_name: account.display_name,
                offline_uuid: account.offline_uuid.ok_or(ImportError::InvalidData)?,
                created_at: account.created_at,
                updated_at: account.updated_at,
            };
            input.validate().map_err(|_| ImportError::InvalidData)?;
            Ok(ImportedIdentity::Offline(input))
        }
        "microsoft" => {
            if !legacy_microsoft_id(&account.account_id)
                || account.offline_uuid.is_some()
                || !account.login_id.as_deref().is_some_and(legacy_login_id)
            {
                return Err(ImportError::InvalidData);
            }
            let input = MicrosoftIdentityImport {
                profile_id: account
                    .minecraft_profile_id
                    .ok_or(ImportError::InvalidData)?,
                display_name: account.display_name,
                created_at: account.created_at,
                updated_at: account.updated_at,
            };
            input.validate().map_err(|_| ImportError::InvalidData)?;
            Ok(ImportedIdentity::Microsoft(input))
        }
        _ => Err(ImportError::InvalidData),
    }
}

#[cfg(test)]
#[path = "metadata_tests.rs"]
mod tests;
