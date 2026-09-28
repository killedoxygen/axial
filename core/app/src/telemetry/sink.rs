use super::event::QueuedEvent;
use serde_json::json;
use std::time::Duration;
use url::{Host, Url};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(3);
pub const DEFAULT_COLLECTOR: &str = "https://eu.i.posthog.com";

#[derive(Clone, Debug)]
pub enum TelemetryEnvironment {
    Development,
    Production,
    Test,
    /// Collector construction validates this label even when built directly.
    Custom(String),
}

impl TelemetryEnvironment {
    pub fn from_label(raw: &str) -> Option<Self> {
        let label = raw.trim().to_ascii_lowercase();
        if label.is_empty()
            || label.len() > 32
            || !label
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        {
            return None;
        }
        Some(match label.as_str() {
            "dev" => Self::Development,
            "production" => Self::Production,
            "test" => Self::Test,
            _ => Self::Custom(label),
        })
    }

    fn label(&self) -> &str {
        match self {
            Self::Development => "dev",
            Self::Production => "production",
            Self::Test => "test",
            Self::Custom(label) => label,
        }
    }
}

/// Credentials and the collector URL deliberately have no Debug implementation.
pub struct CollectorConfig {
    key: String,
    endpoint: Url,
    environment: TelemetryEnvironment,
    client: reqwest::Client,
}

#[derive(Clone, Copy, Debug, thiserror::Error)]
#[error("invalid telemetry collector configuration")]
pub struct CollectorConfigurationError;

impl CollectorConfig {
    /// Explicit construction avoids inheriting the installed application's
    /// telemetry configuration or enabling exports during an isolated rewrite.
    pub fn new(
        key: &str,
        host: &str,
        environment: TelemetryEnvironment,
    ) -> Result<Self, CollectorConfigurationError> {
        let environment = TelemetryEnvironment::from_label(environment.label())
            .ok_or(CollectorConfigurationError)?;
        let key = key.trim();
        if !(8..=128).contains(&key.len())
            || !key.starts_with("phc_")
            || !key
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        {
            return Err(CollectorConfigurationError);
        }
        if host.len() > 2048 {
            return Err(CollectorConfigurationError);
        }
        let mut endpoint = Url::parse(host.trim()).map_err(|_| CollectorConfigurationError)?;
        let loopback = match endpoint.host() {
            Some(Host::Ipv4(ip)) => ip.is_loopback(),
            Some(Host::Ipv6(ip)) => ip.is_loopback(),
            Some(Host::Domain(name)) => name.eq_ignore_ascii_case("localhost"),
            None => return Err(CollectorConfigurationError),
        };
        if !(endpoint.scheme() == "https" || endpoint.scheme() == "http" && loopback)
            || !endpoint.username().is_empty()
            || endpoint.password().is_some()
            || endpoint.query().is_some()
            || endpoint.fragment().is_some()
        {
            return Err(CollectorConfigurationError);
        }
        endpoint.set_path(&format!("{}/batch/", endpoint.path().trim_end_matches('/')));
        let client = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(concat!("axial/", env!("CARGO_PKG_VERSION"), " telemetry"))
            .build()
            .map_err(|_| CollectorConfigurationError)?;
        Ok(Self {
            key: key.to_owned(),
            endpoint,
            environment,
            client,
        })
    }

    pub fn posthog(
        key: &str,
        environment: TelemetryEnvironment,
    ) -> Result<Self, CollectorConfigurationError> {
        Self::new(key, DEFAULT_COLLECTOR, environment)
    }

    pub(super) async fn send(&self, identity: &str, events: Vec<QueuedEvent>) -> bool {
        let body = json!({
            "api_key": self.key,
            "batch": events.into_iter().map(|event| event.batch_item(identity, self.environment.label())).collect::<Vec<_>>(),
        });
        // Never log response bodies, request errors, collector credentials or URLs.
        // Export failure is best effort and cannot recursively emit an exception.
        self.client
            .post(self.endpoint.clone())
            .json(&body)
            .send()
            .await
            .is_ok_and(|response| response.status().is_success())
    }
}
