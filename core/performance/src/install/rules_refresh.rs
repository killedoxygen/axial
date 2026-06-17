use super::manager::{PerformanceManager, active_rules_write};
use super::model::{
    ActiveRules, PERFORMANCE_RULES_URL_ENV, RemoteRulesCandidate, RulesRefreshError,
};
use crate::resolve::validate_manifest;
use crate::rules_cache::{bounded_warning, write_remote_rules_cache};
use crate::signature::{
    RULES_KEY_ID_HEADER, RULES_SIGNATURE_HEADER, signature_metadata_from_header,
};
use crate::status::{RuleChannel, RuleSource, RulesValidation};
use std::path::Path;
use std::time::Duration;

const REMOTE_RULES_TIMEOUT: Duration = Duration::from_secs(10);
const REMOTE_RULES_MAX_BYTES: usize = 1024 * 1024;

impl PerformanceManager {
    pub async fn refresh_rules(
        &self,
    ) -> Result<crate::status::PerformanceRulesStatus, RulesRefreshError> {
        let Some(config_dir) = self.config_dir.as_ref() else {
            return Err(RulesRefreshError::Unconfigured);
        };
        let Some(remote_rules_url) = self.remote_rules_url.as_ref() else {
            return Err(RulesRefreshError::Unconfigured);
        };
        if let Some(warning) = self.remote_rules_verifier.acceptance_warning() {
            self.record_refresh_warning(format!("Remote rules refresh rejected: {warning}"));
            return Ok(self.rules_status());
        }

        match self.fetch_remote_manifest(remote_rules_url).await {
            Ok(candidate) => match self.accept_remote_manifest(config_dir, candidate) {
                Ok(()) => Ok(self.rules_status()),
                Err(error) => {
                    self.record_refresh_warning(remote_rules_refresh_warning("failed", &error));
                    Ok(self.rules_status())
                }
            },
            Err(error) => {
                self.record_refresh_warning(remote_rules_refresh_warning("rejected", &error));
                Ok(self.rules_status())
            }
        }
    }
    async fn fetch_remote_manifest(
        &self,
        remote_rules_url: &str,
    ) -> Result<RemoteRulesCandidate, RulesRefreshError> {
        let response = self
            .rules_client
            .get(remote_rules_url)
            .timeout(REMOTE_RULES_TIMEOUT)
            .send()
            .await?;
        if !response.status().is_success() {
            return Err(RulesRefreshError::HttpStatus(response.status().as_u16()));
        }
        if response
            .content_length()
            .is_some_and(|length| length > REMOTE_RULES_MAX_BYTES as u64)
        {
            return Err(RulesRefreshError::ResponseTooLarge);
        }
        let signature = signature_metadata_from_header(
            response
                .headers()
                .get(RULES_SIGNATURE_HEADER)
                .and_then(|value| value.to_str().ok()),
            response
                .headers()
                .get(RULES_KEY_ID_HEADER)
                .and_then(|value| value.to_str().ok()),
        )?;

        let mut body = Vec::new();
        let mut response = response;
        while let Some(chunk) = response.chunk().await? {
            if body.len().saturating_add(chunk.len()) > REMOTE_RULES_MAX_BYTES {
                return Err(RulesRefreshError::ResponseTooLarge);
            }
            body.extend_from_slice(&chunk);
        }

        let manifest = serde_json::from_slice::<crate::types::Manifest>(&body)?;
        validate_manifest(&manifest)?;
        self.remote_rules_verifier
            .verify_manifest(&manifest, &signature)?;
        Ok(RemoteRulesCandidate {
            manifest,
            signature,
        })
    }

    pub(super) fn accept_remote_manifest(
        &self,
        config_dir: &Path,
        candidate: RemoteRulesCandidate,
    ) -> Result<(), RulesRefreshError> {
        let RemoteRulesCandidate {
            manifest,
            signature,
        } = candidate;
        validate_manifest(&manifest)?;
        self.remote_rules_verifier
            .verify_manifest(&manifest, &signature)?;
        let rules_cache = write_remote_rules_cache(config_dir, &manifest, signature)?;
        let last_refresh_at = rules_cache.updated_at.clone();
        let mut active = active_rules_write(&self.active);
        *active = ActiveRules {
            manifest,
            rule_source: RuleSource::Remote,
            rule_channel: RuleChannel::Remote,
            rules_cache,
            remote_refresh: true,
            last_refresh_at,
            validation: RulesValidation::Valid,
        };
        Ok(())
    }

    pub(super) fn record_refresh_warning(&self, warning: String) {
        let mut active = active_rules_write(&self.active);
        active.rules_cache.warning = Some(bounded_warning(warning));
    }
}

pub(super) fn configured_remote_rules_url() -> Option<String> {
    normalize_remote_rules_url(std::env::var(PERFORMANCE_RULES_URL_ENV).ok())
}

pub(super) fn normalize_remote_rules_url(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

pub(super) fn remote_rules_refresh_warning(action: &str, error: &RulesRefreshError) -> String {
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

pub(super) fn rules_client() -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent("croopor/0.4.0-alpha performance-rules")
        .timeout(REMOTE_RULES_TIMEOUT)
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}
