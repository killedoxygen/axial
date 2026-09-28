//! Captured predecessor rules are candidates, never imported trust policy.

use std::sync::Arc;

use super::{
    ImportError, ImportResult, Inventory,
    model::{RulesImportRequest, RulesImportResponse, RulesImportStatus},
};
use crate::{
    library::ApplicationRootPin,
    performance::rules::{self, CompletedRulesImport, PerformanceRules, PreparedRules},
    tasks::CancellationToken,
};

pub(super) const CACHE: &str = "profile/performance/rules-cache.json";

#[derive(Debug, thiserror::Error)]
pub enum RulesImportError {
    #[error(transparent)]
    Source(#[from] ImportError),
    #[error(transparent)]
    Rules(#[from] rules::RulesImportError),
}

#[derive(Clone)]
pub struct PreparedRulesImport {
    inventory: Arc<Inventory>,
    prepared: PreparedRules,
}

impl Inventory {
    pub(super) fn rules_import_id(&self) -> ImportResult<String> {
        rules::import_id(&self.source_identity()?, self.fingerprint())
            .map_err(|_| ImportError::InvalidData)
    }

    pub(super) fn rules_import_available(&self) -> bool {
        self.rules_inputs().is_ok()
    }

    pub(super) fn validate_rules_completion(
        &self,
        completed: &CompletedRulesImport,
    ) -> ImportResult<()> {
        if !completed.matches(&self.source_identity()?, self.fingerprint())
            || completed.receipt() != self.rules_inputs()?.receipt()
        {
            return Err(ImportError::InvalidData);
        }
        Ok(())
    }

    pub(super) fn prepare_rules(
        self: &Arc<Self>,
        fingerprint: &str,
    ) -> ImportResult<PreparedRulesImport> {
        self.revalidate()?;
        if fingerprint != self.fingerprint() {
            return Err(ImportError::SourceChanged);
        }
        let prepared = self.rules_inputs()?;
        self.revalidate()?;
        Ok(PreparedRulesImport {
            inventory: self.clone(),
            prepared,
        })
    }

    fn rules_inputs(&self) -> ImportResult<PreparedRules> {
        // Alternate names, scratch and nested records are not cache authority.
        if self
            .directory_names()
            .any(|name| name.starts_with("profile/performance/"))
            || self.obligations().iter().any(|record| {
                record.source_record.starts_with("profile/performance/")
                    && (record.source_record != CACHE
                        || record.blocker == super::ImportBlocker::UnsafeFile)
            })
        {
            return Err(ImportError::InvalidData);
        }
        let cache = match self.file_manifests().find(|file| file.relative == CACHE) {
            Some(file) if file.size > axial_performance::RULES_CACHE_MAX_BYTES => {
                return Err(ImportError::LimitExceeded);
            }
            Some(_) => Some(self.record_bytes(CACHE)?),
            None => None,
        };
        let history = super::history::prepare_rules_history(self)?;
        rules::prepare_import(&self.source_identity()?, self.fingerprint(), cache, history)
            .map_err(|_| ImportError::InvalidData)
    }
}

impl PreparedRulesImport {
    /// The caller retains this future through the existing task owner. Source
    /// I/O runs off-runtime after admission to the rules write gate; no file I/O
    /// or provider request runs inside the metadata transaction.
    pub async fn commit(
        &self,
        rules: &PerformanceRules,
        destination: &ApplicationRootPin,
        request: &RulesImportRequest,
        cancel: &CancellationToken,
    ) -> Result<RulesImportResponse, RulesImportError> {
        if request.fingerprint != self.inventory.fingerprint()
            || request.rules_import_id != self.prepared.receipt().rules_import_id
        {
            return Err(ImportError::SourceChanged.into());
        }
        let inventory = self.inventory.clone();
        let destination = destination.clone();
        let check = async move {
            tokio::task::spawn_blocking(move || {
                let root = destination.directory().map_err(ImportError::Io)?;
                inventory.validate_destination_root(&root)
            })
            .await
            .map_err(|_| ImportError::Unavailable)??;
            Ok::<_, RulesImportError>(())
        };
        let commit = rules.commit_import(&self.prepared, cancel, check).await?;
        Ok(RulesImportResponse {
            receipt: commit.receipt,
            already_imported: commit.already_imported,
            stored_cache_matches_import: commit.stored_cache_matches_import,
            cutover_available: false,
        })
    }
}

/// Historical completion is independent of the source volume and current
/// active rules. A later refresh never becomes an import retry or rollback.
pub fn rules_status(
    rules: &PerformanceRules,
    import_id: &str,
) -> Result<RulesImportStatus, RulesImportError> {
    let (receipt, stored_cache_matches_import) = rules.import_status(import_id)?;
    Ok(RulesImportStatus {
        receipt,
        stored_cache_matches_import,
        cutover_available: false,
    })
}
