use std::collections::BTreeSet;
use std::time::Duration;

use reqwest::Url;

/// Origins are supplied by each provider owner, never inferred from a redirect.
#[derive(Clone, Debug)]
pub struct OriginPolicy {
    origins: BTreeSet<String>,
    pub(super) max_redirects: usize,
    allow_loopback_http: bool,
}

impl OriginPolicy {
    pub fn https<I, S>(origins: I, max_redirects: usize) -> Result<Self, DownloadError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        Self::new(origins, max_redirects, false)
    }

    /// Fixture servers must use literal loopback addresses and exact ports.
    #[cfg(any(test, feature = "test-support"))]
    pub fn loopback_for_tests<I, S>(origins: I, max_redirects: usize) -> Result<Self, DownloadError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        Self::new(origins, max_redirects, true)
    }

    fn new<I, S>(
        origins: I,
        max_redirects: usize,
        allow_loopback_http: bool,
    ) -> Result<Self, DownloadError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        if max_redirects > 10 {
            return Err(DownloadError::InvalidPolicy);
        }
        let mut allowed = BTreeSet::new();
        for origin in origins {
            let url = parse_url(origin.as_ref(), allow_loopback_http)?;
            if url.path() != "/" || url.query().is_some() || url.fragment().is_some() {
                return Err(DownloadError::InvalidPolicy);
            }
            allowed.insert(url.origin().ascii_serialization());
        }
        if allowed.is_empty() || allowed.len() > 64 {
            return Err(DownloadError::InvalidPolicy);
        }
        Ok(Self {
            origins: allowed,
            max_redirects,
            allow_loopback_http,
        })
    }

    pub(super) fn admit(&self, value: &str) -> Result<Url, DownloadError> {
        let url = parse_url(value, self.allow_loopback_http)?;
        if !self.origins.contains(&url.origin().ascii_serialization()) {
            return Err(DownloadError::OriginNotAllowed);
        }
        Ok(url)
    }
}

fn parse_url(value: &str, allow_loopback_http: bool) -> Result<Url, DownloadError> {
    if value.len() > 16 * 1024 {
        return Err(DownloadError::InvalidUrl);
    }
    let url = Url::parse(value).map_err(|_| DownloadError::InvalidUrl)?;
    if !url.username().is_empty()
        || url.password().is_some()
        || url.host_str().is_none()
        || url.fragment().is_some()
    {
        return Err(DownloadError::InvalidUrl);
    }
    let loopback_http = allow_loopback_http
        && url.scheme() == "http"
        && match url.host() {
            Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
            Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
            _ => false,
        };
    if url.scheme() != "https" && !loopback_http {
        return Err(DownloadError::InsecureOrigin);
    }
    Ok(url)
}

#[derive(Clone, Debug)]
pub struct ClientConfig {
    pub connect_timeout: Duration,
    pub read_timeout: Duration,
    /// Entire redirect chain, transfer, decompression, and verification.
    pub total_timeout: Duration,
    pub user_agent: String,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            connect_timeout: Duration::from_secs(20),
            read_timeout: Duration::from_secs(30),
            total_timeout: Duration::from_secs(10 * 60),
            user_agent: "axial/0.4".to_owned(),
        }
    }
}

/// Limits apply to the HTTP representation and the decoded artifact separately.
#[derive(Clone, Copy, Debug)]
pub struct ResponseLimits {
    pub max_encoded_bytes: u64,
    pub max_decoded_bytes: u64,
}

impl ResponseLimits {
    pub const fn new(max_encoded_bytes: u64, max_decoded_bytes: u64) -> Self {
        Self {
            max_encoded_bytes,
            max_decoded_bytes,
        }
    }

    pub(super) fn validate(self) -> Result<(), DownloadError> {
        // This interface returns an in-memory body. Large artifact consumers must
        // use explicitly bounded limits; unlimited provider bodies are forbidden.
        const MAX_BUFFERED_BYTES: u64 = 512 * 1024 * 1024;
        if self.max_encoded_bytes == 0
            || self.max_decoded_bytes == 0
            || self.max_encoded_bytes > MAX_BUFFERED_BYTES
            || self.max_decoded_bytes > MAX_BUFFERED_BYTES
        {
            return Err(DownloadError::InvalidPolicy);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HashAlgorithm {
    Sha1,
    Sha256,
    Sha512,
}

#[derive(Clone, Debug)]
pub struct Checksum {
    pub(super) algorithm: HashAlgorithm,
    pub(super) bytes: Vec<u8>,
}

impl Checksum {
    pub fn from_hex(algorithm: HashAlgorithm, value: &str) -> Result<Self, DownloadError> {
        let length = match algorithm {
            HashAlgorithm::Sha1 => 20,
            HashAlgorithm::Sha256 => 32,
            HashAlgorithm::Sha512 => 64,
        };
        if value.len() != length * 2 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(DownloadError::InvalidChecksum);
        }
        Ok(Self {
            algorithm,
            bytes: hex::decode(value).map_err(|_| DownloadError::InvalidChecksum)?,
        })
    }

    pub fn algorithm(&self) -> HashAlgorithm {
        self.algorithm
    }
}

/// Missing integrity metadata must be a deliberate, provider-specific choice.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnhashedReason {
    ProviderMetadata,
    ProviderDoesNotPublishChecksum,
}

#[derive(Clone, Debug)]
pub enum IntegrityPolicy {
    Checksum {
        checksum: Checksum,
        expected_size: Option<u64>,
    },
    Unhashed {
        reason: UnhashedReason,
        expected_size: Option<u64>,
    },
}

impl IntegrityPolicy {
    pub(super) fn expected_size(&self) -> Option<u64> {
        match self {
            Self::Checksum { expected_size, .. } | Self::Unhashed { expected_size, .. } => {
                *expected_size
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IntegrityEvidence {
    ChecksumMatched { algorithm: HashAlgorithm },
    Unhashed { reason: UnhashedReason },
}

/// No caller-controlled headers or credentials can leak across provider origins.
#[derive(Clone, Debug)]
pub struct DownloadRequest {
    pub(super) url: String,
    pub(super) body: Option<serde_json::Value>,
    pub(super) origins: OriginPolicy,
    pub(super) limits: ResponseLimits,
    pub(super) integrity: IntegrityPolicy,
}

impl DownloadRequest {
    pub fn get(
        url: impl Into<String>,
        origins: OriginPolicy,
        limits: ResponseLimits,
        integrity: IntegrityPolicy,
    ) -> Self {
        Self {
            url: url.into(),
            body: None,
            origins,
            limits,
            integrity,
        }
    }

    /// Intended for bounded, read-only provider query endpoints.
    pub fn post_json(
        url: impl Into<String>,
        body: serde_json::Value,
        origins: OriginPolicy,
        limits: ResponseLimits,
        integrity: IntegrityPolicy,
    ) -> Self {
        Self {
            url: url.into(),
            body: Some(body),
            origins,
            limits,
            integrity,
        }
    }
}

/// Errors deliberately omit URLs, query strings, provider bodies and credentials.
#[derive(Clone, Debug, thiserror::Error, PartialEq, Eq)]
pub enum DownloadError {
    #[error("provider URL is invalid")]
    InvalidUrl,
    #[error("provider source must use HTTPS")]
    InsecureOrigin,
    #[error("provider origin is not permitted")]
    OriginNotAllowed,
    #[error("provider request policy is invalid")]
    InvalidPolicy,
    #[error("provider checksum is invalid")]
    InvalidChecksum,
    #[error("provider redirect limit exceeded")]
    RedirectLimit,
    #[error("provider redirect is invalid")]
    InvalidRedirect,
    #[error("provider query redirect would change or disclose the request body")]
    RedirectMethod,
    #[error("provider returned HTTP status {status}")]
    HttpStatus { status: u16 },
    #[error("provider request timed out")]
    Timeout,
    #[error("provider request was cancelled")]
    Cancelled,
    #[error("provider connection failed")]
    Network,
    #[error("provider response exceeds the encoded byte limit of {limit}")]
    EncodedLimitExceeded { limit: u64 },
    #[error("provider response exceeds the decoded byte limit of {limit}")]
    DecodedLimitExceeded { limit: u64 },
    #[error("provider response uses an unsupported content encoding")]
    UnsupportedEncoding,
    #[error("provider compressed response is invalid or incomplete")]
    InvalidEncoding,
    #[error("provider response is incomplete")]
    IncompleteBody,
    #[error("provider artifact size does not match its metadata")]
    SizeMismatch,
    #[error("provider artifact checksum does not match its metadata")]
    ChecksumMismatch,
    #[error("provider request body exceeds its byte limit")]
    RequestBodyTooLarge,
    #[error("provider response allocation failed")]
    AllocationFailed,
}

impl DownloadError {
    pub fn is_retryable(&self) -> bool {
        matches!(self, Self::Timeout | Self::Network | Self::IncompleteBody)
            || matches!(self, Self::HttpStatus { status } if *status == 408 || *status == 429 || (500..=599).contains(status))
    }
}
