use super::manager::{ACTIVE_RULES_LOCK_INVARIANT, PerformanceManager, PerformanceRulesAuthority};
use super::model::{ActiveRules, RemoteRulesCandidate, RulesRefreshError, VerifiedRemoteRules};
use crate::resolve::validate_manifest;
use crate::rules_cache::{
    RulesCacheState, RulesCacheStatus, bounded_warning, remote_rules_snapshot,
};
use crate::signature::{
    RULES_KEY_ID_HEADER, RULES_SIGNATURE_HEADER, signature_metadata_from_header,
};
use crate::status::{RuleChannel, RuleSource, RulesValidation};
use std::future::Future;
use std::time::Duration;

const REMOTE_RULES_TIMEOUT: Duration = Duration::from_secs(10);
const REMOTE_RULES_MAX_BYTES: u64 = 1024 * 1024;

impl PerformanceRulesAuthority {
    pub fn mutation_allowed(&self) -> bool {
        self.manager.rules_mutation_allowed
    }

    pub async fn fetch_remote_rules(&self) -> Result<VerifiedRemoteRules, RulesRefreshError> {
        let manager = &self.manager;
        let Some(remote_rules_url) = manager.remote_rules_url.as_ref() else {
            return Err(RulesRefreshError::Unconfigured);
        };
        if manager.remote_rules_verifier.acceptance_warning().is_some() {
            let error = match &manager.remote_rules_verifier {
                crate::signature::RemoteRulesVerifier::MissingPublicKey => {
                    crate::signature::RulesSignatureError::MissingPublicKey
                }
                _ => crate::signature::RulesSignatureError::InvalidPublicKey,
            };
            return Err(error.into());
        }
        let candidate = manager.fetch_remote_manifest(remote_rules_url).await?;
        self.verify_remote_manifest(candidate)
    }

    fn verify_remote_manifest(
        &self,
        candidate: RemoteRulesCandidate,
    ) -> Result<VerifiedRemoteRules, RulesRefreshError> {
        let manager = &self.manager;
        let RemoteRulesCandidate {
            manifest,
            signature,
        } = candidate;
        validate_manifest(&manifest)?;
        manager
            .remote_rules_verifier
            .verify_manifest(&manifest, &signature)?;
        let snapshot = remote_rules_snapshot(&manifest, signature);
        let rules_cache = RulesCacheStatus::from_snapshot(&snapshot, RulesCacheState::Recorded);
        let last_refresh_at = rules_cache.updated_at.clone();
        Ok(VerifiedRemoteRules {
            active: ActiveRules {
                manifest,
                rule_source: RuleSource::Remote,
                rule_channel: RuleChannel::Remote,
                rules_cache,
                remote_refresh: true,
                last_refresh_at,
                validation: RulesValidation::Valid,
            },
            snapshot,
        })
    }

    #[cfg(test)]
    pub fn verify_remote_rules_for_test(
        &self,
        manifest: crate::types::Manifest,
        signature: crate::signature::RulesSignatureMetadata,
    ) -> Result<VerifiedRemoteRules, RulesRefreshError> {
        self.verify_remote_manifest(RemoteRulesCandidate {
            manifest,
            signature,
        })
    }

    pub async fn settle_remote_rules<Error, Persisted>(
        &self,
        candidate: VerifiedRemoteRules,
        persisted: Persisted,
    ) -> Result<crate::status::PerformanceRulesStatus, Error>
    where
        Persisted: Future<Output = Result<(), Error>>,
    {
        persisted.await?;
        *self
            .manager
            .active
            .write()
            .expect(ACTIVE_RULES_LOCK_INVARIANT) = candidate.active;
        Ok(self.rules_status())
    }

    pub fn record_refresh_warning(&self, warning: String) {
        let mut active = self
            .manager
            .active
            .write()
            .expect(ACTIVE_RULES_LOCK_INVARIANT);
        active.rules_cache.warning = Some(bounded_warning(warning));
    }

    pub fn rules_status(&self) -> crate::status::PerformanceRulesStatus {
        self.manager.rules_status()
    }
}

impl PerformanceManager {
    async fn fetch_remote_manifest(
        &self,
        remote_rules_url: &str,
    ) -> Result<RemoteRulesCandidate, RulesRefreshError> {
        let body = axial_minecraft::http::send(
            self.rules_client
                .get(remote_rules_url)
                .timeout(REMOTE_RULES_TIMEOUT),
            REMOTE_RULES_MAX_BYTES,
        )
        .await?;
        let signature = signature_metadata_from_header(
            body.headers()
                .get(RULES_SIGNATURE_HEADER)
                .and_then(|value| value.to_str().ok()),
            body.headers()
                .get(RULES_KEY_ID_HEADER)
                .and_then(|value| value.to_str().ok()),
        )?;
        let body = body.bytes().await?;

        let manifest = serde_json::from_slice::<crate::types::Manifest>(&body)?;
        validate_manifest(&manifest)?;
        self.remote_rules_verifier
            .verify_manifest(&manifest, &signature)?;
        Ok(RemoteRulesCandidate {
            manifest,
            signature,
        })
    }
}

pub(super) fn normalize_remote_rules_url(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

pub fn remote_rules_refresh_warning(action: &str, error: &RulesRefreshError) -> String {
    match error {
        RulesRefreshError::Request(_) => {
            format!("Remote rules refresh {action}: request failed; using previously active rules.")
        }
        RulesRefreshError::Cache(_) => format!(
            "Remote rules refresh {action}: remote rules cache could not be persisted; using previously active rules."
        ),
        RulesRefreshError::Unconfigured
        | RulesRefreshError::HttpStatus(_)
        | RulesRefreshError::ResponseTooLarge
        | RulesRefreshError::Parse(_)
        | RulesRefreshError::Validation(_)
        | RulesRefreshError::Signature(_) => format!("Remote rules refresh {action}: {error}"),
    }
}

/// Rules are signature-verified, so an operator-configured plain-HTTP source remains admitted.
pub(super) fn rules_client() -> reqwest::Client {
    axial_minecraft::http::artifact_client().clone()
}
