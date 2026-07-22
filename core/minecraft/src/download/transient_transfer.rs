use axial_fs::{
    FileCapability, TransientCreationObligation, TransientDestination,
    TransientDestinationCancelObligation, TransientDestinationCancelOutcome,
    TransientDiscardObligation, TransientDiscardOutcome, TransientPublicationBatch,
    TransientPublicationBatchObligation, TransientPublicationBatchOutcome,
    TransientPublicationMember, TransientStage, TransientStageCreateOutcome,
    TransientStageSealed,
};
use futures_util::FutureExt as _;
use reqwest::header::{ACCEPT_ENCODING, CONTENT_ENCODING};
use sha1::{Digest as _, Sha1};
use sha2::Sha512;
use std::fmt;
use std::io::{self, Read, Seek, SeekFrom};
use std::num::NonZeroU64;
use std::panic::AssertUnwindSafe;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

const FRAME_BYTES: usize = 64 * 1024;
const FRAME_CAPACITY: usize = 8;
const MAX_ATTEMPTS: usize = 8;
const MAX_RETRY_DELAYS: usize = MAX_ATTEMPTS - 1;
const MAX_FAILURE_EVENTS: usize = MAX_ATTEMPTS;
const MAX_REDIRECTS: usize = 8;
const MAX_TRANSFER_ORIGINS: usize = 8;
const MAX_CONNECT_TIMEOUT: Duration = Duration::from_secs(2 * 60);
const MAX_IDLE_READ_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const MAX_REQUEST_TIMEOUT: Duration = Duration::from_secs(6 * 60 * 60);
const MAX_RETRY_DELAY: Duration = Duration::from_secs(30);
const MAX_RETRY_WINDOW: Duration = Duration::from_secs(2 * 60);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransferByteContract {
    Exact(NonZeroU64),
    AtMost(NonZeroU64),
    Below(NonZeroU64),
}

impl TransferByteContract {
    fn limit(self) -> u64 {
        match self {
            Self::Exact(value) | Self::AtMost(value) | Self::Below(value) => value.get(),
        }
    }

    fn admits_partial(self, observed: u64) -> bool {
        match self {
            Self::Exact(limit) | Self::AtMost(limit) => observed <= limit.get(),
            Self::Below(limit) => observed < limit.get(),
        }
    }

    fn admits_final(self, observed: u64) -> bool {
        match self {
            Self::Exact(expected) => observed == expected.get(),
            Self::AtMost(limit) => observed <= limit.get(),
            Self::Below(limit) => observed < limit.get(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransferDigestAlgorithm {
    Sha1,
    Sha512,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TransferDigestParseError {
    InvalidSha1,
    InvalidSha512,
}

impl fmt::Display for TransferDigestParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidSha1 => formatter.write_str("invalid SHA-1 digest"),
            Self::InvalidSha512 => formatter.write_str("invalid SHA-512 digest"),
        }
    }
}

impl std::error::Error for TransferDigestParseError {}

#[derive(Clone, Default, Eq, PartialEq)]
pub struct ExpectedTransferDigests {
    sha1: Option<[u8; 20]>,
    sha512: Option<[u8; 64]>,
}

impl fmt::Debug for ExpectedTransferDigests {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ExpectedTransferDigests")
            .field("sha1", &self.sha1.is_some())
            .field("sha512", &self.sha512.is_some())
            .finish()
    }
}

impl ExpectedTransferDigests {
    pub fn none() -> Self {
        Self::default()
    }

    pub fn sha1(digest: [u8; 20]) -> Self {
        Self {
            sha1: Some(digest),
            sha512: None,
        }
    }

    pub fn sha512(digest: [u8; 64]) -> Self {
        Self {
            sha1: None,
            sha512: Some(digest),
        }
    }

    pub fn both(sha1: [u8; 20], sha512: [u8; 64]) -> Self {
        Self {
            sha1: Some(sha1),
            sha512: Some(sha512),
        }
    }

    pub fn from_hex(
        sha1: Option<&str>,
        sha512: Option<&str>,
    ) -> Result<Self, TransferDigestParseError> {
        Ok(Self {
            sha1: sha1
                .map(|value| {
                    parse_hex_digest::<20>(value)
                        .ok_or(TransferDigestParseError::InvalidSha1)
                })
                .transpose()?,
            sha512: sha512
                .map(|value| {
                    parse_hex_digest::<64>(value)
                        .ok_or(TransferDigestParseError::InvalidSha512)
                })
                .transpose()?,
        })
    }

    pub fn expected_sha1(&self) -> Option<&[u8; 20]> {
        self.sha1.as_ref()
    }

    pub fn expected_sha512(&self) -> Option<&[u8; 64]> {
        self.sha512.as_ref()
    }

    fn is_authenticated(&self) -> bool {
        self.sha1.is_some() || self.sha512.is_some()
    }
}

fn parse_hex_digest<const N: usize>(value: &str) -> Option<[u8; N]> {
    if value.len() != N * 2 || !value.is_ascii() {
        return None;
    }
    let mut digest = [0_u8; N];
    for (index, pair) in value.as_bytes().chunks_exact(2).enumerate() {
        let high = parse_hex_nibble(pair[0])?;
        let low = parse_hex_nibble(pair[1])?;
        digest[index] = (high << 4) | low;
    }
    Some(digest)
}

fn parse_hex_nibble(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransferContractError {
    MissingDigest,
}

impl fmt::Display for TransferContractError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("authenticated transfer contract requires a digest")
    }
}

impl std::error::Error for TransferContractError {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TransferContract {
    bytes: TransferByteContract,
    digests: ExpectedTransferDigests,
}

impl TransferContract {
    pub fn unauthenticated_at_most(limit: NonZeroU64) -> Self {
        Self {
            bytes: TransferByteContract::AtMost(limit),
            digests: ExpectedTransferDigests::none(),
        }
    }

    pub fn authenticated_exact(
        size: NonZeroU64,
        digests: ExpectedTransferDigests,
    ) -> Result<Self, TransferContractError> {
        Self::authenticated(TransferByteContract::Exact(size), digests)
    }

    pub fn authenticated_below(
        limit: NonZeroU64,
        digests: ExpectedTransferDigests,
    ) -> Result<Self, TransferContractError> {
        Self::authenticated(TransferByteContract::Below(limit), digests)
    }

    fn authenticated(
        bytes: TransferByteContract,
        digests: ExpectedTransferDigests,
    ) -> Result<Self, TransferContractError> {
        if !digests.is_authenticated() {
            return Err(TransferContractError::MissingDigest);
        }
        Ok(Self { bytes, digests })
    }

    pub fn bytes(&self) -> TransferByteContract {
        self.bytes
    }

    pub fn digests(&self) -> &ExpectedTransferDigests {
        &self.digests
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RetryPolicyError {
    TooManyAttempts,
    ZeroDelay,
    DelayExceedsMaximum,
    RetryWindowExceedsMaximum,
}

impl fmt::Display for RetryPolicyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooManyAttempts => {
                formatter.write_str("transfer retry policy exceeds eight attempts")
            }
            Self::ZeroDelay => formatter.write_str("transfer retry delay must be positive"),
            Self::DelayExceedsMaximum => {
                formatter.write_str("transfer retry delay exceeds its maximum")
            }
            Self::RetryWindowExceedsMaximum => {
                formatter.write_str("transfer retry window exceeds its maximum")
            }
        }
    }
}

impl std::error::Error for RetryPolicyError {}

#[derive(Clone)]
pub struct RetryPolicy {
    delays: [Duration; MAX_RETRY_DELAYS],
    delay_count: u8,
    classifier: fn(&TransferFailureKind) -> bool,
}

impl fmt::Debug for RetryPolicy {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RetryPolicy")
            .field("attempts", &(usize::from(self.delay_count) + 1))
            .finish()
    }
}

impl RetryPolicy {
    pub fn none() -> Self {
        Self {
            delays: [Duration::ZERO; MAX_RETRY_DELAYS],
            delay_count: 0,
            classifier: |_| false,
        }
    }

    pub fn classified(
        delays: &[Duration],
        classifier: fn(&TransferFailureKind) -> bool,
    ) -> Result<Self, RetryPolicyError> {
        if delays.len() > MAX_RETRY_DELAYS {
            return Err(RetryPolicyError::TooManyAttempts);
        }
        let mut retry_window = Duration::ZERO;
        for delay in delays {
            if delay.is_zero() {
                return Err(RetryPolicyError::ZeroDelay);
            }
            if *delay > MAX_RETRY_DELAY {
                return Err(RetryPolicyError::DelayExceedsMaximum);
            }
            retry_window = retry_window
                .checked_add(*delay)
                .filter(|window| *window <= MAX_RETRY_WINDOW)
                .ok_or(RetryPolicyError::RetryWindowExceedsMaximum)?;
        }
        let mut owned = [Duration::ZERO; MAX_RETRY_DELAYS];
        owned[..delays.len()].copy_from_slice(delays);
        Ok(Self {
            delays: owned,
            delay_count: delays.len() as u8,
            classifier,
        })
    }

    fn delay_after(&self, attempt: usize) -> Option<Duration> {
        (attempt < usize::from(self.delay_count)).then(|| self.delays[attempt])
    }

    fn permits_retry(&self, failure: &TransferFailureKind) -> bool {
        failure.is_policy_retryable() && (self.classifier)(failure)
    }
}

#[must_use = "transfer targets retain an admitted destination reservation"]
pub struct CreateOnlyTransferTarget {
    destination: TransientDestination,
}

impl fmt::Debug for CreateOnlyTransferTarget {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CreateOnlyTransferTarget")
            .finish_non_exhaustive()
    }
}

impl CreateOnlyTransferTarget {
    pub fn new(destination: TransientDestination) -> Self {
        Self { destination }
    }

    pub fn cancel(self) -> TransientDestinationCancelOutcome {
        self.destination.cancel()
    }
}

#[must_use = "transfer targets retain an admitted destination reservation"]
pub struct SourceOnlyTransferTarget {
    destination: TransientDestination,
}

impl fmt::Debug for SourceOnlyTransferTarget {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SourceOnlyTransferTarget")
            .finish_non_exhaustive()
    }
}

impl SourceOnlyTransferTarget {
    pub fn new(destination: TransientDestination) -> Self {
        Self { destination }
    }

    pub fn cancel(self) -> TransientDestinationCancelOutcome {
        self.destination.cancel()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransferFailureKind {
    Cancelled,
    Network,
    RequestPolicy,
    ProviderStatus(u16),
    ContentEncodingRejected,
    ContentLengthContractMismatch {
        declared: u64,
        contract: TransferByteContract,
    },
    ContentLengthMismatch {
        declared: u64,
        observed: u64,
    },
    ByteLimitExceeded {
        limit: u64,
        observed: u64,
    },
    SizeMismatch {
        expected: u64,
        observed: u64,
    },
    ByteCountOverflow,
    ProducerWorkerMismatch {
        producer: u64,
        writer: u64,
    },
    DigestMismatch(TransferDigestAlgorithm),
    StageCreate(io::ErrorKind),
    StageWrite(io::ErrorKind),
    StageSeal(io::ErrorKind),
    ChannelClosed,
    WorkerStopped,
}

impl TransferFailureKind {
    fn is_policy_retryable(self) -> bool {
        matches!(
            self,
            Self::Network | Self::ProviderStatus(408 | 425 | 429 | 500..=599)
        )
    }

    fn is_writer_local(self) -> bool {
        matches!(
            self,
            Self::ByteLimitExceeded { .. }
                | Self::SizeMismatch { .. }
                | Self::ByteCountOverflow
                | Self::ProducerWorkerMismatch { .. }
                | Self::DigestMismatch(_)
                | Self::StageCreate(_)
                | Self::StageWrite(_)
                | Self::StageSeal(_)
        )
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TransferFailureEvent {
    attempt: u8,
    kind: TransferFailureKind,
}

impl TransferFailureEvent {
    pub fn attempt(&self) -> u8 {
        self.attempt
    }

    pub fn kind(&self) -> TransferFailureKind {
        self.kind
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TransferFailureReport {
    attempts: u8,
    last: TransferFailureKind,
    events: Vec<TransferFailureEvent>,
}

impl TransferFailureReport {
    pub fn attempts(&self) -> u8 {
        self.attempts
    }

    pub fn last(&self) -> TransferFailureKind {
        self.last
    }

    pub fn events(&self) -> &[TransferFailureEvent] {
        &self.events
    }

    fn single(kind: TransferFailureKind) -> Self {
        Self {
            attempts: 0,
            last: kind,
            events: vec![TransferFailureEvent { attempt: 0, kind }],
        }
    }
}

struct FailureTrace {
    attempts: u8,
    last: TransferFailureKind,
    events: Vec<TransferFailureEvent>,
}

impl FailureTrace {
    fn new() -> Self {
        Self {
            attempts: 0,
            last: TransferFailureKind::WorkerStopped,
            events: Vec::with_capacity(MAX_FAILURE_EVENTS),
        }
    }

    fn record(&mut self, attempt: usize, kind: TransferFailureKind) {
        self.attempts = u8::try_from(attempt + 1).unwrap_or(MAX_ATTEMPTS as u8);
        self.record_terminal(kind);
    }

    fn record_terminal(&mut self, kind: TransferFailureKind) {
        self.last = kind;
        if self.events.len() < MAX_FAILURE_EVENTS {
            self.events.push(TransferFailureEvent {
                attempt: self.attempts,
                kind,
            });
        }
    }

    fn report(&self) -> TransferFailureReport {
        TransferFailureReport {
            attempts: self.attempts,
            last: self.last,
            events: self.events.clone(),
        }
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct VerifiedTransferDigests {
    sha1: Option<[u8; 20]>,
    sha512: Option<[u8; 64]>,
}

impl fmt::Debug for VerifiedTransferDigests {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("VerifiedTransferDigests")
            .field("sha1", &self.sha1.is_some())
            .field("sha512", &self.sha512.is_some())
            .finish()
    }
}

impl VerifiedTransferDigests {
    pub fn sha1(&self) -> Option<&[u8; 20]> {
        self.sha1.as_ref()
    }

    pub fn sha512(&self) -> Option<&[u8; 64]> {
        self.sha512.as_ref()
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct TransferReport {
    attempts: u8,
    bytes: u64,
    declared_length: Option<u64>,
    digests: VerifiedTransferDigests,
}

impl fmt::Debug for TransferReport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TransferReport")
            .field("attempts", &self.attempts)
            .field("bytes", &self.bytes)
            .field("declared_length", &self.declared_length)
            .field("sha1", &self.digests.sha1.is_some())
            .field("sha512", &self.digests.sha512.is_some())
            .finish()
    }
}

impl TransferReport {
    pub fn attempts(&self) -> u8 {
        self.attempts
    }

    pub fn bytes(&self) -> u64 {
        self.bytes
    }

    pub fn declared_length(&self) -> Option<u64> {
        self.declared_length
    }

    pub fn digests(&self) -> &VerifiedTransferDigests {
        &self.digests
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransferOriginError {
    UserInfo,
    UnsupportedScheme,
    MissingHost,
}

impl fmt::Display for TransferOriginError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UserInfo => formatter.write_str("transfer origins cannot contain userinfo"),
            Self::UnsupportedScheme => {
                formatter.write_str("transfer origin scheme is not admitted")
            }
            Self::MissingHost => formatter.write_str("transfer origin host is missing"),
        }
    }
}

impl std::error::Error for TransferOriginError {}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum TransferOriginScheme {
    Https,
    #[cfg(test)]
    LoopbackHttp,
}

#[derive(Clone, Eq, Hash, PartialEq)]
pub struct TransferOrigin {
    scheme: TransferOriginScheme,
    host: Box<str>,
    port: u16,
}

impl fmt::Debug for TransferOrigin {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TransferOrigin")
            .field("scheme", &self.scheme)
            .field("port", &self.port)
            .finish_non_exhaustive()
    }
}

impl TransferOrigin {
    pub fn from_url(url: &reqwest::Url) -> Result<Self, TransferOriginError> {
        if url.scheme() != "https" {
            return Err(TransferOriginError::UnsupportedScheme);
        }
        Self::validated(url, TransferOriginScheme::Https)
    }

    fn validated(
        url: &reqwest::Url,
        scheme: TransferOriginScheme,
    ) -> Result<Self, TransferOriginError> {
        if !url.username().is_empty() || url.password().is_some() {
            return Err(TransferOriginError::UserInfo);
        }
        Ok(Self {
            scheme,
            host: url
                .host_str()
                .ok_or(TransferOriginError::MissingHost)?
                .into(),
            port: url
                .port_or_known_default()
                .expect("admitted HTTP schemes have an effective port"),
        })
    }

    fn admits(&self, url: &reqwest::Url) -> bool {
        let scheme_matches = match self.scheme {
            TransferOriginScheme::Https => url.scheme() == "https",
            #[cfg(test)]
            TransferOriginScheme::LoopbackHttp => {
                url.scheme() == "http"
                    && url
                        .host_str()
                        .and_then(|host| host.parse::<std::net::IpAddr>().ok())
                        .is_some_and(|address| address.is_loopback())
            }
        };
        url.username().is_empty()
            && url.password().is_none()
            && scheme_matches
            && url.host_str().is_some_and(|host| host == &*self.host)
            && url.port_or_known_default() == Some(self.port)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransferTimeoutKind {
    Connect,
    IdleRead,
    Request,
}

impl fmt::Display for TransferTimeoutKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Connect => formatter.write_str("connect"),
            Self::IdleRead => formatter.write_str("idle-read"),
            Self::Request => formatter.write_str("request"),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransferClientConfigError {
    ZeroTimeout(TransferTimeoutKind),
    TimeoutExceedsMaximum(TransferTimeoutKind),
    TimeoutExceedsRequest(TransferTimeoutKind),
    MissingOrigins,
    TooManyOrigins,
    DuplicateOrigin,
}

impl fmt::Display for TransferClientConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroTimeout(kind) => write!(formatter, "{kind} timeout must be positive"),
            Self::TimeoutExceedsMaximum(kind) => {
                write!(formatter, "{kind} timeout exceeds the transfer maximum")
            }
            Self::TimeoutExceedsRequest(kind) => {
                write!(formatter, "{kind} timeout exceeds the overall request timeout")
            }
            Self::MissingOrigins => formatter.write_str("transfer origin set is empty"),
            Self::TooManyOrigins => formatter.write_str("transfer origin set exceeds its maximum"),
            Self::DuplicateOrigin => {
                formatter.write_str("transfer origin set contains a duplicate")
            }
        }
    }
}

impl std::error::Error for TransferClientConfigError {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TransferClientConfig {
    connect_timeout: Duration,
    idle_read_timeout: Duration,
    request_timeout: Duration,
    origins: Vec<TransferOrigin>,
}

impl TransferClientConfig {
    pub fn bounded(
        connect_timeout: Duration,
        idle_read_timeout: Duration,
        request_timeout: Duration,
        origins: Vec<TransferOrigin>,
    ) -> Result<Self, TransferClientConfigError> {
        validate_timeout(
            TransferTimeoutKind::Connect,
            connect_timeout,
            MAX_CONNECT_TIMEOUT,
        )?;
        validate_timeout(
            TransferTimeoutKind::IdleRead,
            idle_read_timeout,
            MAX_IDLE_READ_TIMEOUT,
        )?;
        validate_timeout(
            TransferTimeoutKind::Request,
            request_timeout,
            MAX_REQUEST_TIMEOUT,
        )?;
        for (kind, timeout) in [
            (TransferTimeoutKind::Connect, connect_timeout),
            (TransferTimeoutKind::IdleRead, idle_read_timeout),
        ] {
            if timeout > request_timeout {
                return Err(TransferClientConfigError::TimeoutExceedsRequest(kind));
            }
        }
        if origins.is_empty() {
            return Err(TransferClientConfigError::MissingOrigins);
        }
        if origins.len() > MAX_TRANSFER_ORIGINS {
            return Err(TransferClientConfigError::TooManyOrigins);
        }
        for (index, origin) in origins.iter().enumerate() {
            if origins[..index].contains(origin) {
                return Err(TransferClientConfigError::DuplicateOrigin);
            }
        }
        Ok(Self {
            connect_timeout,
            idle_read_timeout,
            request_timeout,
            origins,
        })
    }

    pub fn connect_timeout(&self) -> Duration {
        self.connect_timeout
    }

    pub fn idle_read_timeout(&self) -> Duration {
        self.idle_read_timeout
    }

    pub fn request_timeout(&self) -> Duration {
        self.request_timeout
    }

    pub fn origin_count(&self) -> usize {
        self.origins.len()
    }
}

fn validate_timeout(
    kind: TransferTimeoutKind,
    timeout: Duration,
    maximum: Duration,
) -> Result<(), TransferClientConfigError> {
    if timeout.is_zero() {
        Err(TransferClientConfigError::ZeroTimeout(kind))
    } else if timeout > maximum {
        Err(TransferClientConfigError::TimeoutExceedsMaximum(kind))
    } else {
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TransferClientBuildError;

impl fmt::Display for TransferClientBuildError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("managed transfer client could not be built")
    }
}

impl std::error::Error for TransferClientBuildError {}

#[derive(Clone)]
pub struct TransferClient {
    inner: reqwest::Client,
    origins: Arc<[TransferOrigin]>,
}

impl fmt::Debug for TransferClient {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TransferClient")
            .finish_non_exhaustive()
    }
}

impl TransferClient {
    pub fn build(config: TransferClientConfig) -> Result<Self, TransferClientBuildError> {
        let origins: Arc<[TransferOrigin]> = config.origins.into();
        let redirect_origins = Arc::clone(&origins);
        reqwest::Client::builder()
            .connect_timeout(config.connect_timeout)
            .read_timeout(config.idle_read_timeout)
            .timeout(config.request_timeout)
            .redirect(reqwest::redirect::Policy::custom(move |attempt| {
                if attempt.previous().len() > MAX_REDIRECTS
                    || !redirect_origins
                        .iter()
                        .any(|origin| origin.admits(attempt.url()))
                {
                    attempt.error(TransferRedirectPolicyError)
                } else {
                    attempt.follow()
                }
            }))
            .referer(false)
            .retry(reqwest::retry::never())
            .no_gzip()
            .no_brotli()
            .no_deflate()
            .no_zstd()
            .build()
            .map(|inner| Self { inner, origins })
            .map_err(|_| TransferClientBuildError)
    }

    fn admits_url(&self, url: &reqwest::Url) -> bool {
        self.origins.iter().any(|origin| origin.admits(url))
    }
}

#[derive(Debug)]
struct TransferRedirectPolicyError;

impl fmt::Display for TransferRedirectPolicyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("redirect violates managed transfer policy")
    }
}

impl std::error::Error for TransferRedirectPolicyError {}

struct TransferCancellationShared {
    cancelled: AtomicBool,
    changed: tokio::sync::watch::Sender<bool>,
}

impl TransferCancellationShared {
    fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        let _ = self.changed.send(true);
    }
}

pub struct TransferCancellationSender {
    shared: Arc<TransferCancellationShared>,
}

impl fmt::Debug for TransferCancellationSender {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TransferCancellationSender")
            .finish_non_exhaustive()
    }
}

impl TransferCancellationSender {
    pub fn cancel(&self) {
        self.shared.cancel();
    }
}

impl Drop for TransferCancellationSender {
    fn drop(&mut self) {
        self.shared.cancel();
    }
}

#[derive(Clone)]
pub struct TransferCancellation {
    shared: Arc<TransferCancellationShared>,
    changed: tokio::sync::watch::Receiver<bool>,
}

impl fmt::Debug for TransferCancellation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TransferCancellation")
            .finish_non_exhaustive()
    }
}

pub fn transfer_cancellation_channel() -> (TransferCancellationSender, TransferCancellation) {
    let (changed, changed_rx) = tokio::sync::watch::channel(false);
    let shared = Arc::new(TransferCancellationShared {
        cancelled: AtomicBool::new(false),
        changed,
    });
    (
        TransferCancellationSender {
            shared: Arc::clone(&shared),
        },
        TransferCancellation {
            shared,
            changed: changed_rx,
        },
    )
}

impl TransferCancellation {
    pub fn is_cancelled(&self) -> bool {
        self.shared.cancelled.load(Ordering::Acquire)
    }

    async fn cancelled(&mut self) {
        loop {
            if self.is_cancelled() || *self.changed.borrow_and_update() {
                return;
            }
            if self.changed.changed().await.is_err() {
                return;
            }
        }
    }

    async fn wait<T>(&mut self, future: impl std::future::Future<Output = T>) -> Option<T> {
        tokio::select! {
            biased;
            () = self.cancelled() => None,
            result = future => Some(result),
        }
    }

    fn thread_cancellation(&self) -> TransferThreadCancellation {
        TransferThreadCancellation {
            shared: Arc::clone(&self.shared),
        }
    }
}

#[derive(Clone)]
struct TransferThreadCancellation {
    shared: Arc<TransferCancellationShared>,
}

impl TransferThreadCancellation {
    fn is_cancelled(&self) -> bool {
        self.shared.cancelled.load(Ordering::Acquire)
    }
}

#[must_use = "transfer outcomes retain verified data or unsettled effect authority"]
pub enum TransferOutcome<T> {
    Complete(T),
    Failed(TransferFailureReport),
    CleanupPending(TransferCleanupObligation),
    Unsettled(TransferFailureReport),
}

impl<T> fmt::Debug for TransferOutcome<T> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let variant = match self {
            Self::Complete(_) => "Complete",
            Self::Failed(_) => "Failed",
            Self::CleanupPending(_) => "CleanupPending",
            Self::Unsettled(_) => "Unsettled",
        };
        formatter
            .debug_struct("TransferOutcome")
            .field("variant", &variant)
            .finish()
    }
}

enum TransferCleanupState {
    Creation(TransientCreationObligation),
    Discard(TransientDiscardObligation),
    DestinationCancel(TransientDestinationCancelObligation),
}

#[must_use = "pending transfer cleanup authority must be reconciled"]
pub struct TransferCleanupObligation {
    report: TransferFailureReport,
    state: Option<TransferCleanupState>,
}

impl fmt::Debug for TransferCleanupObligation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TransferCleanupObligation")
            .field("report", &self.report)
            .finish_non_exhaustive()
    }
}

#[must_use = "transfer cleanup resolution must be terminal or retained"]
pub enum TransferCleanupResolution {
    Discarded(TransferFailureReport),
    Pending(TransferCleanupObligation),
}

impl fmt::Debug for TransferCleanupResolution {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let variant = match self {
            Self::Discarded(_) => "Discarded",
            Self::Pending(_) => "Pending",
        };
        formatter
            .debug_struct("TransferCleanupResolution")
            .field("variant", &variant)
            .finish()
    }
}

impl TransferCleanupObligation {
    pub fn report(&self) -> &TransferFailureReport {
        &self.report
    }

    pub fn reconcile(mut self) -> TransferCleanupResolution {
        let state = self
            .state
            .take()
            .expect("transfer cleanup obligation retains its state");
        match state {
            TransferCleanupState::Creation(obligation) => match obligation.reconcile() {
                TransientStageCreateOutcome::Created(stage) => {
                    self.reconcile_discard(stage.discard())
                }
                TransientStageCreateOutcome::NoEffect { destination, .. } => {
                    self.reconcile_destination_cancel(destination.cancel())
                }
                TransientStageCreateOutcome::Pending(obligation) => {
                    self.state = Some(TransferCleanupState::Creation(obligation));
                    TransferCleanupResolution::Pending(self)
                }
            },
            TransferCleanupState::Discard(obligation) => {
                self.reconcile_discard(obligation.reconcile())
            }
            TransferCleanupState::DestinationCancel(obligation) => {
                self.reconcile_destination_cancel(obligation.reconcile())
            }
        }
    }

    fn reconcile_discard(
        mut self,
        outcome: TransientDiscardOutcome,
    ) -> TransferCleanupResolution {
        match outcome {
            TransientDiscardOutcome::Discarded(destination) => {
                self.reconcile_destination_cancel(destination.cancel())
            }
            TransientDiscardOutcome::Pending(obligation) => {
                self.state = Some(TransferCleanupState::Discard(obligation));
                TransferCleanupResolution::Pending(self)
            }
        }
    }

    fn reconcile_destination_cancel(
        mut self,
        outcome: TransientDestinationCancelOutcome,
    ) -> TransferCleanupResolution {
        match outcome {
            TransientDestinationCancelOutcome::Cancelled => {
                TransferCleanupResolution::Discarded(self.report)
            }
            TransientDestinationCancelOutcome::Pending(obligation) => {
                self.state = Some(TransferCleanupState::DestinationCancel(obligation));
                TransferCleanupResolution::Pending(self)
            }
        }
    }
}

#[must_use = "verified create-only data must be published or explicitly discarded"]
pub struct VerifiedCreateOnly {
    sealed: TransientStageSealed,
    report: TransferReport,
}

impl fmt::Debug for VerifiedCreateOnly {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("VerifiedCreateOnly")
            .field("report", &self.report)
            .finish_non_exhaustive()
    }
}

impl VerifiedCreateOnly {
    pub fn report(&self) -> &TransferReport {
        &self.report
    }

    /// Publishes one independently terminal singleton destination.
    ///
    /// Grouped content, performance, and runtime publication must use the
    /// later batch authority; this outcome cannot prove group atomicity.
    pub fn publish_create_new(self) -> TransferPublicationOutcome {
        let Self { sealed, report } = self;
        let batch = match TransientPublicationBatch::new(vec![sealed]) {
            Ok(batch) => batch,
            Err(failure) => {
                let error_kind = failure.error().kind();
                return TransferPublicationOutcome::NoEffect {
                    error_kind,
                    verified: Self {
                        sealed: take_singleton(failure.into_stages()),
                        report,
                    },
                };
            }
        };
        map_singleton_publication(batch.publish_create_new(), report)
    }

    pub fn discard(self) -> VerifiedTransferDiscardOutcome {
        let Self { sealed, report } = self;
        verified_discard(report, sealed.discard())
    }
}

#[must_use = "verified source data must be consumed and explicitly discarded"]
pub struct VerifiedSource {
    sealed: TransientStageSealed,
    report: TransferReport,
}

impl fmt::Debug for VerifiedSource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("VerifiedSource")
            .field("report", &self.report)
            .finish_non_exhaustive()
    }
}

impl VerifiedSource {
    pub fn report(&self) -> &TransferReport {
        &self.report
    }

    pub fn discard(self) -> VerifiedTransferDiscardOutcome {
        let Self { sealed, report } = self;
        verified_discard(report, sealed.discard())
    }
}

impl Read for VerifiedSource {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.sealed.read(buffer)
    }
}

impl Seek for VerifiedSource {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        self.sealed.seek(position)
    }
}

#[must_use = "publication outcomes retain verified data or exact native authority"]
pub enum TransferPublicationOutcome {
    Published {
        file: FileCapability,
        report: TransferReport,
    },
    NoEffect {
        error_kind: io::ErrorKind,
        verified: VerifiedCreateOnly,
    },
    Pending(TransferPublicationObligation),
}

impl fmt::Debug for TransferPublicationOutcome {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let variant = match self {
            Self::Published { .. } => "Published",
            Self::NoEffect { .. } => "NoEffect",
            Self::Pending(_) => "Pending",
        };
        formatter
            .debug_struct("TransferPublicationOutcome")
            .field("variant", &variant)
            .finish()
    }
}

#[must_use = "pending transfer publication authority must be reconciled"]
pub struct TransferPublicationObligation {
    report: TransferReport,
    obligation: Option<TransientPublicationBatchObligation>,
}

impl fmt::Debug for TransferPublicationObligation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TransferPublicationObligation")
            .field("report", &self.report)
            .finish_non_exhaustive()
    }
}

impl TransferPublicationObligation {
    pub fn report(&self) -> &TransferReport {
        &self.report
    }

    pub fn reconcile(mut self) -> TransferPublicationOutcome {
        let obligation = self
            .obligation
            .take()
            .expect("transfer publication obligation retains native authority");
        map_singleton_publication(obligation.reconcile(), self.report)
    }
}

fn map_singleton_publication(
    outcome: TransientPublicationBatchOutcome,
    report: TransferReport,
) -> TransferPublicationOutcome {
    match outcome {
        TransientPublicationBatchOutcome::Published(files) => {
            TransferPublicationOutcome::Published {
                file: take_singleton(files),
                report,
            }
        }
        TransientPublicationBatchOutcome::NoEffect { error, batch } => {
            TransferPublicationOutcome::NoEffect {
                error_kind: error.kind(),
                verified: VerifiedCreateOnly {
                    sealed: take_singleton(batch.into_stages()),
                    report,
                },
            }
        }
        TransientPublicationBatchOutcome::Partial { error, members } => {
            match take_singleton(members) {
                TransientPublicationMember::Published(file) => {
                    TransferPublicationOutcome::Published { file, report }
                }
                TransientPublicationMember::Unpublished(sealed) => {
                    TransferPublicationOutcome::NoEffect {
                        error_kind: error.kind(),
                        verified: VerifiedCreateOnly { sealed, report },
                    }
                }
            }
        }
        TransientPublicationBatchOutcome::Pending(obligation) => {
            TransferPublicationOutcome::Pending(TransferPublicationObligation {
                report,
                obligation: Some(obligation),
            })
        }
    }
}

fn take_singleton<T>(mut values: Vec<T>) -> T {
    assert!(
        values.len() == 1,
        "singleton publication returned an invalid member count"
    );
    values
        .pop()
        .expect("singleton publication retains one member")
}

#[must_use = "verified discard outcomes retain pending native authority"]
pub enum VerifiedTransferDiscardOutcome {
    Discarded(TransferReport),
    Pending(VerifiedTransferDiscardObligation),
}

impl fmt::Debug for VerifiedTransferDiscardOutcome {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let variant = match self {
            Self::Discarded(_) => "Discarded",
            Self::Pending(_) => "Pending",
        };
        formatter
            .debug_struct("VerifiedTransferDiscardOutcome")
            .field("variant", &variant)
            .finish()
    }
}

#[must_use = "pending verified discard authority must be reconciled"]
pub struct VerifiedTransferDiscardObligation {
    report: TransferReport,
    state: Option<VerifiedTransferDiscardState>,
}

enum VerifiedTransferDiscardState {
    Discard(TransientDiscardObligation),
    DestinationCancel(TransientDestinationCancelObligation),
}

impl fmt::Debug for VerifiedTransferDiscardObligation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("VerifiedTransferDiscardObligation")
            .field("report", &self.report)
            .finish_non_exhaustive()
    }
}

impl VerifiedTransferDiscardObligation {
    pub fn report(&self) -> &TransferReport {
        &self.report
    }

    pub fn reconcile(mut self) -> VerifiedTransferDiscardOutcome {
        let state = self
            .state
            .take()
            .expect("verified discard obligation retains native authority");
        match state {
            VerifiedTransferDiscardState::Discard(obligation) => {
                self.reconcile_discard(obligation.reconcile())
            }
            VerifiedTransferDiscardState::DestinationCancel(obligation) => {
                self.reconcile_destination_cancel(obligation.reconcile())
            }
        }
    }

    fn reconcile_discard(
        mut self,
        outcome: TransientDiscardOutcome,
    ) -> VerifiedTransferDiscardOutcome {
        match outcome {
            TransientDiscardOutcome::Discarded(destination) => {
                self.reconcile_destination_cancel(destination.cancel())
            }
            TransientDiscardOutcome::Pending(obligation) => {
                self.state = Some(VerifiedTransferDiscardState::Discard(obligation));
                VerifiedTransferDiscardOutcome::Pending(self)
            }
        }
    }

    fn reconcile_destination_cancel(
        mut self,
        outcome: TransientDestinationCancelOutcome,
    ) -> VerifiedTransferDiscardOutcome {
        match outcome {
            TransientDestinationCancelOutcome::Cancelled => {
                VerifiedTransferDiscardOutcome::Discarded(self.report)
            }
            TransientDestinationCancelOutcome::Pending(obligation) => {
                self.state = Some(VerifiedTransferDiscardState::DestinationCancel(obligation));
                VerifiedTransferDiscardOutcome::Pending(self)
            }
        }
    }
}

fn verified_discard(
    report: TransferReport,
    outcome: TransientDiscardOutcome,
) -> VerifiedTransferDiscardOutcome {
    match outcome {
        TransientDiscardOutcome::Discarded(destination) => {
            verified_destination_cancel(report, destination.cancel())
        }
        TransientDiscardOutcome::Pending(obligation) => {
            VerifiedTransferDiscardOutcome::Pending(VerifiedTransferDiscardObligation {
                report,
                state: Some(VerifiedTransferDiscardState::Discard(obligation)),
            })
        }
    }
}

fn verified_destination_cancel(
    report: TransferReport,
    outcome: TransientDestinationCancelOutcome,
) -> VerifiedTransferDiscardOutcome {
    match outcome {
        TransientDestinationCancelOutcome::Cancelled => {
            VerifiedTransferDiscardOutcome::Discarded(report)
        }
        TransientDestinationCancelOutcome::Pending(obligation) => {
            VerifiedTransferDiscardOutcome::Pending(VerifiedTransferDiscardObligation {
                report,
                state: Some(VerifiedTransferDiscardState::DestinationCancel(obligation)),
            })
        }
    }
}

#[must_use = "transfer tasks must be joined before their owner publishes a terminal state"]
pub struct TransferTask<T> {
    cancellation: Arc<TransferCancellationShared>,
    join: Option<tokio::task::JoinHandle<TransferOutcome<T>>>,
}

impl<T> fmt::Debug for TransferTask<T> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TransferTask")
            .finish_non_exhaustive()
    }
}

impl<T: Send + 'static> TransferTask<T> {
    pub fn cancel(&self) {
        self.cancellation.cancel();
    }

    pub async fn cancel_and_join(self) -> TransferOutcome<T> {
        self.cancel();
        self.join().await
    }

    pub async fn join(mut self) -> TransferOutcome<T> {
        let join = self
            .join
            .take()
            .expect("transfer task retains its supervisor join authority");
        match join.await {
            Ok(outcome) => outcome,
            Err(_) => TransferOutcome::Unsettled(TransferFailureReport::single(
                TransferFailureKind::WorkerStopped,
            )),
        }
    }
}

impl<T> Drop for TransferTask<T> {
    fn drop(&mut self) {
        if self.join.is_some() {
            self.cancellation.cancel();
        }
    }
}

pub fn start_create_only_transfer(
    client: TransferClient,
    url: reqwest::Url,
    target: CreateOnlyTransferTarget,
    contract: TransferContract,
    retry: RetryPolicy,
    cancellation: TransferCancellation,
) -> TransferTask<VerifiedCreateOnly> {
    let task_cancellation = Arc::clone(&cancellation.shared);
    let join = tokio::spawn(async move {
        map_transfer_outcome(
            run_transfer(
                client,
                url,
                target.destination,
                contract,
                retry,
                cancellation,
            )
            .await,
            |completed| VerifiedCreateOnly {
                    sealed: completed.sealed,
                    report: completed.report,
            },
        )
    });
    TransferTask {
        cancellation: task_cancellation,
        join: Some(join),
    }
}

pub fn start_source_transfer(
    client: TransferClient,
    url: reqwest::Url,
    target: SourceOnlyTransferTarget,
    contract: TransferContract,
    retry: RetryPolicy,
    cancellation: TransferCancellation,
) -> TransferTask<VerifiedSource> {
    let task_cancellation = Arc::clone(&cancellation.shared);
    let join = tokio::spawn(async move {
        map_transfer_outcome(
            run_transfer(
                client,
                url,
                target.destination,
                contract,
                retry,
                cancellation,
            )
            .await,
            |completed| VerifiedSource {
                sealed: completed.sealed,
                report: completed.report,
            },
        )
    });
    TransferTask {
        cancellation: task_cancellation,
        join: Some(join),
    }
}

fn map_transfer_outcome<T, U>(
    outcome: TransferOutcome<T>,
    complete: impl FnOnce(T) -> U,
) -> TransferOutcome<U> {
    match outcome {
        TransferOutcome::Complete(value) => TransferOutcome::Complete(complete(value)),
        TransferOutcome::Failed(report) => TransferOutcome::Failed(report),
        TransferOutcome::CleanupPending(obligation) => {
            TransferOutcome::CleanupPending(obligation)
        }
        TransferOutcome::Unsettled(report) => TransferOutcome::Unsettled(report),
    }
}

struct CompletedTransfer {
    sealed: TransientStageSealed,
    report: TransferReport,
}

async fn run_transfer(
    client: TransferClient,
    url: reqwest::Url,
    destination: TransientDestination,
    contract: TransferContract,
    retry: RetryPolicy,
    mut cancellation: TransferCancellation,
) -> TransferOutcome<CompletedTransfer> {
    let mut failures = FailureTrace::new();
    let mut destination = destination;
    if !client.admits_url(&url) {
        failures.record_terminal(TransferFailureKind::RequestPolicy);
        return terminal_failure(failures.report(), destination);
    }
    for attempt in 0..MAX_ATTEMPTS {
        if cancellation.is_cancelled() {
            failures.record_terminal(TransferFailureKind::Cancelled);
            return terminal_failure(failures.report(), destination);
        }
        match run_attempt(
            &client,
            &url,
            destination,
            &contract,
            cancellation.clone(),
        )
        .await
        {
            AttemptOutcome::Verified {
                sealed,
                verification,
            } => {
                return TransferOutcome::Complete(CompletedTransfer {
                    sealed,
                    report: TransferReport {
                        attempts: u8::try_from(attempt + 1).unwrap_or(MAX_ATTEMPTS as u8),
                        bytes: verification.bytes,
                        declared_length: verification.declared_length,
                        digests: verification.digests,
                    },
                });
            }
            AttemptOutcome::Discarded {
                failure,
                destination: returned_destination,
            } => {
                failures.record(attempt, failure);
                let Some(delay) = retry.delay_after(attempt) else {
                    return terminal_failure(failures.report(), returned_destination);
                };
                if !retry.permits_retry(&failure) {
                    return terminal_failure(failures.report(), returned_destination);
                }
                if cancellation.wait(tokio::time::sleep(delay)).await.is_none() {
                    failures.record_terminal(TransferFailureKind::Cancelled);
                    return terminal_failure(failures.report(), returned_destination);
                }
                destination = returned_destination;
            }
            AttemptOutcome::CleanupPending { failure, state } => {
                failures.record(attempt, failure);
                return TransferOutcome::CleanupPending(TransferCleanupObligation {
                    report: failures.report(),
                    state: Some(state),
                });
            }
            AttemptOutcome::Unsettled(failure) => {
                failures.record(attempt, failure);
                return TransferOutcome::Unsettled(failures.report());
            }
        }
    }
    unreachable!("retry policy limits transfer execution to eight attempts")
}

fn terminal_failure(
    report: TransferFailureReport,
    destination: TransientDestination,
) -> TransferOutcome<CompletedTransfer> {
    match destination.cancel() {
        TransientDestinationCancelOutcome::Cancelled => TransferOutcome::Failed(report),
        TransientDestinationCancelOutcome::Pending(obligation) => {
            TransferOutcome::CleanupPending(TransferCleanupObligation {
                report,
                state: Some(TransferCleanupState::DestinationCancel(obligation)),
            })
        }
    }
}

enum AttemptOutcome {
    Verified {
        sealed: TransientStageSealed,
        verification: WriterVerification,
    },
    Discarded {
        failure: TransferFailureKind,
        destination: TransientDestination,
    },
    CleanupPending {
        failure: TransferFailureKind,
        state: TransferCleanupState,
    },
    Unsettled(TransferFailureKind),
}

enum ProducerExit {
    Finished,
    Failed(TransferFailureKind),
}

enum WriterMessage {
    Frame(Box<[u8]>),
    Finish {
        producer_bytes: u64,
        declared_length: Option<u64>,
    },
}

enum WriterExit {
    Verified {
        sealed: TransientStageSealed,
        verification: WriterVerification,
    },
    Discarded {
        failure: TransferFailureKind,
        destination: TransientDestination,
    },
    CleanupPending {
        failure: TransferFailureKind,
        state: TransferCleanupState,
    },
}

struct WriterVerification {
    bytes: u64,
    declared_length: Option<u64>,
    digests: VerifiedTransferDigests,
}

struct AttemptCancellationGuard {
    cancelled: Arc<AtomicBool>,
    armed: bool,
}

impl AttemptCancellationGuard {
    fn new() -> Self {
        Self {
            cancelled: Arc::new(AtomicBool::new(false)),
            armed: true,
        }
    }

    fn flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.cancelled)
    }

    fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for AttemptCancellationGuard {
    fn drop(&mut self) {
        if self.armed {
            self.cancel();
        }
    }
}

struct WriterCancellation {
    transfer: TransferThreadCancellation,
    attempt: Arc<AtomicBool>,
}

impl WriterCancellation {
    fn is_cancelled(&self) -> bool {
        self.transfer.is_cancelled() || self.attempt.load(Ordering::Acquire)
    }
}

async fn run_attempt(
    client: &TransferClient,
    url: &reqwest::Url,
    destination: TransientDestination,
    contract: &TransferContract,
    cancellation: TransferCancellation,
) -> AttemptOutcome {
    let (messages, receiver) = tokio::sync::mpsc::channel(FRAME_CAPACITY);
    let (ready, readiness) = tokio::sync::oneshot::channel();
    let mut attempt_cancellation = AttemptCancellationGuard::new();
    let writer_cancellation = WriterCancellation {
        transfer: cancellation.thread_cancellation(),
        attempt: attempt_cancellation.flag(),
    };
    let writer_contract = contract.clone();
    let writer = tokio::task::spawn_blocking(move || {
        run_writer(
            destination,
            writer_contract,
            receiver,
            ready,
            writer_cancellation,
        )
    });

    let producer = AssertUnwindSafe(run_producer(
        client,
        url,
        contract,
        messages.clone(),
        readiness,
        cancellation.clone(),
    ))
    .catch_unwind()
    .await;
    let producer_panicked = producer.is_err();
    let producer = producer.unwrap_or(ProducerExit::Failed(TransferFailureKind::WorkerStopped));
    if matches!(producer, ProducerExit::Failed(_)) {
        attempt_cancellation.cancel();
    }
    drop(messages);
    let writer_exit = writer.await;
    let producer = if cancellation.is_cancelled() {
        ProducerExit::Failed(TransferFailureKind::Cancelled)
    } else {
        producer
    };
    attempt_cancellation.disarm();

    let Ok(writer_exit) = writer_exit else {
        return AttemptOutcome::Unsettled(TransferFailureKind::WorkerStopped);
    };
    if producer_panicked {
        return merge_panicked_producer(writer_exit);
    }
    merge_attempt_outcome(producer, writer_exit)
}

async fn run_producer(
    client: &TransferClient,
    url: &reqwest::Url,
    contract: &TransferContract,
    messages: tokio::sync::mpsc::Sender<WriterMessage>,
    mut readiness: tokio::sync::oneshot::Receiver<()>,
    mut cancellation: TransferCancellation,
) -> ProducerExit {
    match wait_for_writer(&mut cancellation, &messages, &mut readiness).await {
        Some(Ok(())) => {}
        Some(Err(_)) => return ProducerExit::Failed(TransferFailureKind::WorkerStopped),
        None => return ProducerExit::Failed(wait_interruption(&cancellation, &messages)),
    }

    let request = client
        .inner
        .get(url.clone())
        .headers(identity_request_headers())
        .send();
    let mut response = match wait_for_writer(&mut cancellation, &messages, request).await {
        Some(Ok(response)) => response,
        Some(Err(error)) => return ProducerExit::Failed(classify_request_error(&error)),
        None => return ProducerExit::Failed(wait_interruption(&cancellation, &messages)),
    };
    if let Some(failure) = provider_status_failure(response.status()) {
        return ProducerExit::Failed(failure);
    }
    if !response_has_identity_encoding(&response) {
        return ProducerExit::Failed(TransferFailureKind::ContentEncodingRejected);
    }
    let declared_length = response.content_length();
    if let Some(declared) = declared_length {
        if !contract.bytes.admits_final(declared) {
            return ProducerExit::Failed(
                TransferFailureKind::ContentLengthContractMismatch {
                    declared,
                    contract: contract.bytes,
                },
            );
        }
    }

    let mut produced = 0_u64;
    loop {
        let chunk = match wait_for_writer(&mut cancellation, &messages, response.chunk()).await {
            Some(Ok(chunk)) => chunk,
            Some(Err(error)) => return ProducerExit::Failed(classify_request_error(&error)),
            None => return ProducerExit::Failed(wait_interruption(&cancellation, &messages)),
        };
        let Some(chunk) = chunk else {
            break;
        };
        for slice in chunk.chunks(FRAME_BYTES) {
            produced = match admit_bytes(contract.bytes, produced, slice.len()) {
                Ok(produced) => produced,
                Err(failure) => return ProducerExit::Failed(failure),
            };
            let frame = slice.to_vec().into_boxed_slice();
            match wait_for_writer(
                &mut cancellation,
                &messages,
                messages.send(WriterMessage::Frame(frame)),
            )
            .await
            {
                Some(Ok(())) => {}
                Some(Err(_)) => {
                    return ProducerExit::Failed(TransferFailureKind::ChannelClosed);
                }
                None => {
                    return ProducerExit::Failed(wait_interruption(&cancellation, &messages));
                }
            }
        }
    }

    if let Some(declared) = declared_length {
        if declared != produced {
            return ProducerExit::Failed(TransferFailureKind::ContentLengthMismatch {
                declared,
                observed: produced,
            });
        }
    }
    if !contract.bytes.admits_final(produced) {
        return ProducerExit::Failed(final_size_failure(contract.bytes, produced));
    }
    match wait_for_writer(
        &mut cancellation,
        &messages,
        messages.send(WriterMessage::Finish {
            producer_bytes: produced,
            declared_length,
        }),
    )
    .await
    {
        Some(Ok(())) => ProducerExit::Finished,
        Some(Err(_)) => ProducerExit::Failed(TransferFailureKind::ChannelClosed),
        None => ProducerExit::Failed(wait_interruption(&cancellation, &messages)),
    }
}

async fn wait_for_writer<T>(
    cancellation: &mut TransferCancellation,
    messages: &tokio::sync::mpsc::Sender<WriterMessage>,
    future: impl std::future::Future<Output = T>,
) -> Option<T> {
    tokio::select! {
        biased;
        () = cancellation.cancelled() => None,
        result = future => Some(result),
        () = messages.closed() => None,
    }
}

fn wait_interruption(
    cancellation: &TransferCancellation,
    messages: &tokio::sync::mpsc::Sender<WriterMessage>,
) -> TransferFailureKind {
    if cancellation.is_cancelled() {
        TransferFailureKind::Cancelled
    } else if messages.is_closed() {
        TransferFailureKind::ChannelClosed
    } else {
        TransferFailureKind::WorkerStopped
    }
}

fn classify_request_error(error: &reqwest::Error) -> TransferFailureKind {
    if error.is_builder() || error.is_redirect() {
        TransferFailureKind::RequestPolicy
    } else {
        TransferFailureKind::Network
    }
}

fn provider_status_failure(status: reqwest::StatusCode) -> Option<TransferFailureKind> {
    (status != reqwest::StatusCode::OK)
        .then(|| TransferFailureKind::ProviderStatus(status.as_u16()))
}

fn identity_request_headers() -> reqwest::header::HeaderMap {
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        ACCEPT_ENCODING,
        reqwest::header::HeaderValue::from_static("identity"),
    );
    headers
}

fn response_has_identity_encoding(response: &reqwest::Response) -> bool {
    headers_have_identity_encoding(response.headers())
}

fn headers_have_identity_encoding(headers: &reqwest::header::HeaderMap) -> bool {
    let mut values = headers.get_all(CONTENT_ENCODING).iter();
    let Some(first) = values.next() else {
        return true;
    };
    std::iter::once(first).chain(values).all(|value| {
        value.to_str().is_ok_and(|encoding| {
            let mut tokens = encoding.split(',').map(str::trim).peekable();
            tokens.peek().is_some()
                && tokens.all(|token| token.eq_ignore_ascii_case("identity"))
        })
    })
}

fn admit_bytes(
    contract: TransferByteContract,
    current: u64,
    additional: usize,
) -> Result<u64, TransferFailureKind> {
    let additional = u64::try_from(additional).map_err(|_| TransferFailureKind::ByteCountOverflow)?;
    let observed = current
        .checked_add(additional)
        .ok_or(TransferFailureKind::ByteCountOverflow)?;
    if contract.admits_partial(observed) {
        Ok(observed)
    } else {
        Err(TransferFailureKind::ByteLimitExceeded {
            limit: contract.limit(),
            observed,
        })
    }
}

fn final_size_failure(
    contract: TransferByteContract,
    observed: u64,
) -> TransferFailureKind {
    match contract {
        TransferByteContract::Exact(expected) => TransferFailureKind::SizeMismatch {
            expected: expected.get(),
            observed,
        },
        TransferByteContract::AtMost(limit) | TransferByteContract::Below(limit) => {
            TransferFailureKind::ByteLimitExceeded {
                limit: limit.get(),
                observed,
            }
        }
    }
}

fn run_writer(
    destination: TransientDestination,
    contract: TransferContract,
    mut receiver: tokio::sync::mpsc::Receiver<WriterMessage>,
    ready: tokio::sync::oneshot::Sender<()>,
    cancellation: WriterCancellation,
) -> WriterExit {
    if cancellation.is_cancelled() {
        return WriterExit::Discarded {
            failure: TransferFailureKind::Cancelled,
            destination,
        };
    }
    let mut stage = match destination.create_stage() {
        TransientStageCreateOutcome::Created(stage) => stage,
        TransientStageCreateOutcome::NoEffect { error, destination } => {
            return WriterExit::Discarded {
                failure: TransferFailureKind::StageCreate(error.kind()),
                destination,
            };
        }
        TransientStageCreateOutcome::Pending(obligation) => {
            return WriterExit::CleanupPending {
                failure: TransferFailureKind::StageCreate(obligation.error().kind()),
                state: TransferCleanupState::Creation(obligation),
            };
        }
    };
    if cancellation.is_cancelled() || ready.send(()).is_err() {
        return discard_writer_stage(stage, TransferFailureKind::Cancelled);
    }

    let mut written = 0_u64;
    let mut hashers = WriterHashers::new(&contract.digests);
    loop {
        if cancellation.is_cancelled() {
            return discard_writer_stage(stage, TransferFailureKind::Cancelled);
        }
        let Some(message) = receiver.blocking_recv() else {
            let failure = if cancellation.is_cancelled() {
                TransferFailureKind::Cancelled
            } else {
                TransferFailureKind::ChannelClosed
            };
            return discard_writer_stage(stage, failure);
        };
        if cancellation.is_cancelled() {
            return discard_writer_stage(stage, TransferFailureKind::Cancelled);
        }
        match message {
            WriterMessage::Frame(frame) => {
                written = match admit_bytes(contract.bytes, written, frame.len()) {
                    Ok(written) => written,
                    Err(failure) => return discard_writer_stage(stage, failure),
                };
                if let Err(error) = stage.write_all(&frame) {
                    return discard_writer_stage(
                        stage,
                        TransferFailureKind::StageWrite(error.kind()),
                    );
                }
                if cancellation.is_cancelled() {
                    return discard_writer_stage(stage, TransferFailureKind::Cancelled);
                }
                hashers.update(&frame);
            }
            WriterMessage::Finish {
                producer_bytes,
                declared_length,
            } => {
                if producer_bytes != written {
                    return discard_writer_stage(
                        stage,
                        TransferFailureKind::ProducerWorkerMismatch {
                            producer: producer_bytes,
                            writer: written,
                        },
                    );
                }
                if let Some(declared) = declared_length {
                    if declared != written {
                        return discard_writer_stage(
                            stage,
                            TransferFailureKind::ContentLengthMismatch {
                                declared,
                                observed: written,
                            },
                        );
                    }
                }
                if !contract.bytes.admits_final(written) {
                    return discard_writer_stage(
                        stage,
                        final_size_failure(contract.bytes, written),
                    );
                }
                let digests = hashers.finish();
                if let Some(expected) = contract.digests.sha1.as_ref() {
                    if digests.sha1.as_ref() != Some(expected) {
                        return discard_writer_stage(
                            stage,
                            TransferFailureKind::DigestMismatch(TransferDigestAlgorithm::Sha1),
                        );
                    }
                }
                if let Some(expected) = contract.digests.sha512.as_ref() {
                    if digests.sha512.as_ref() != Some(expected) {
                        return discard_writer_stage(
                            stage,
                            TransferFailureKind::DigestMismatch(TransferDigestAlgorithm::Sha512),
                        );
                    }
                }
                if cancellation.is_cancelled() {
                    return discard_writer_stage(stage, TransferFailureKind::Cancelled);
                }
                let sealed = match stage.seal() {
                    Ok(sealed) => sealed,
                    Err(failure) => {
                        let kind = failure.error().kind();
                        return discard_writer_stage(
                            failure.into_stage(),
                            TransferFailureKind::StageSeal(kind),
                        );
                    }
                };
                return WriterExit::Verified {
                    sealed,
                    verification: WriterVerification {
                        bytes: written,
                        declared_length,
                        digests,
                    },
                };
            }
        }
    }
}

struct WriterHashers {
    sha1: Option<Sha1>,
    sha512: Option<Sha512>,
}

impl WriterHashers {
    fn new(expected: &ExpectedTransferDigests) -> Self {
        Self {
            sha1: expected.sha1.is_some().then(Sha1::new),
            sha512: expected.sha512.is_some().then(Sha512::new),
        }
    }

    fn update(&mut self, bytes: &[u8]) {
        if let Some(hasher) = self.sha1.as_mut() {
            hasher.update(bytes);
        }
        if let Some(hasher) = self.sha512.as_mut() {
            hasher.update(bytes);
        }
    }

    fn finish(self) -> VerifiedTransferDigests {
        VerifiedTransferDigests {
            sha1: self.sha1.map(|hasher| hasher.finalize().into()),
            sha512: self.sha512.map(|hasher| hasher.finalize().into()),
        }
    }
}

fn discard_writer_stage(stage: TransientStage, failure: TransferFailureKind) -> WriterExit {
    match stage.discard() {
        TransientDiscardOutcome::Discarded(destination) => WriterExit::Discarded {
            failure,
            destination,
        },
        TransientDiscardOutcome::Pending(obligation) => WriterExit::CleanupPending {
            failure,
            state: TransferCleanupState::Discard(obligation),
        },
    }
}

fn merge_attempt_outcome(producer: ProducerExit, writer: WriterExit) -> AttemptOutcome {
    match writer {
        WriterExit::CleanupPending {
            failure: writer_failure,
            state,
        } => AttemptOutcome::CleanupPending {
            failure: select_attempt_failure(producer, writer_failure),
            state,
        },
        WriterExit::Discarded {
            failure: writer_failure,
            destination,
        } => AttemptOutcome::Discarded {
            failure: select_attempt_failure(producer, writer_failure),
            destination,
        },
        WriterExit::Verified {
            sealed,
            verification,
        } => match producer {
            ProducerExit::Finished => AttemptOutcome::Verified {
                sealed,
                verification,
            },
            ProducerExit::Failed(failure) => match sealed.discard() {
                TransientDiscardOutcome::Discarded(destination) => AttemptOutcome::Discarded {
                    failure,
                    destination,
                },
                TransientDiscardOutcome::Pending(obligation) => AttemptOutcome::CleanupPending {
                    failure,
                    state: TransferCleanupState::Discard(obligation),
                },
            },
        },
    }
}

fn select_attempt_failure(
    producer: ProducerExit,
    writer_failure: TransferFailureKind,
) -> TransferFailureKind {
    match producer {
        ProducerExit::Finished => writer_failure,
        ProducerExit::Failed(_) if writer_failure.is_writer_local() => writer_failure,
        ProducerExit::Failed(producer_failure) => producer_failure,
    }
}

fn merge_panicked_producer(writer: WriterExit) -> AttemptOutcome {
    match writer {
        WriterExit::CleanupPending { state, .. } => AttemptOutcome::CleanupPending {
            failure: TransferFailureKind::WorkerStopped,
            state,
        },
        WriterExit::Discarded { destination, .. } => {
            unsettled_after_destination_cancel(destination.cancel())
        }
        WriterExit::Verified { sealed, .. } => match sealed.discard() {
            TransientDiscardOutcome::Discarded(destination) => {
                unsettled_after_destination_cancel(destination.cancel())
            }
            TransientDiscardOutcome::Pending(obligation) => AttemptOutcome::CleanupPending {
                failure: TransferFailureKind::WorkerStopped,
                state: TransferCleanupState::Discard(obligation),
            },
        },
    }
}

fn unsettled_after_destination_cancel(
    outcome: TransientDestinationCancelOutcome,
) -> AttemptOutcome {
    match outcome {
        TransientDestinationCancelOutcome::Cancelled => {
            AttemptOutcome::Unsettled(TransferFailureKind::WorkerStopped)
        }
        TransientDestinationCancelOutcome::Pending(obligation) => {
            AttemptOutcome::CleanupPending {
                failure: TransferFailureKind::WorkerStopped,
                state: TransferCleanupState::DestinationCancel(obligation),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axial_fs::{LeafName, RootRevokeOutcome, RootSession, RootSessionAcquireOutcome};
    use std::io::{Read as _, Seek as _};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    impl TransferOrigin {
        fn from_loopback_http_for_test(
            url: &reqwest::Url,
        ) -> Result<Self, TransferOriginError> {
            let is_loopback_ip = url
                .host_str()
                .and_then(|host| host.parse::<std::net::IpAddr>().ok())
                .is_some_and(|address| address.is_loopback());
            if url.scheme() != "http" || !is_loopback_ip {
                return Err(TransferOriginError::UnsupportedScheme);
            }
            Self::validated(url, TransferOriginScheme::LoopbackHttp)
        }
    }

    #[test]
    fn digest_metadata_is_canonicalized_to_typed_bytes() {
        let parsed = ExpectedTransferDigests::from_hex(
            Some(&"A5".repeat(20)),
            Some(&"0b".repeat(64)),
        )
        .expect("valid typed digests");
        assert_eq!(parsed.expected_sha1(), Some(&[0xa5; 20]));
        assert_eq!(parsed.expected_sha512(), Some(&[0x0b; 64]));
        assert_eq!(
            ExpectedTransferDigests::from_hex(Some("a5"), None),
            Err(TransferDigestParseError::InvalidSha1)
        );
        assert_eq!(
            ExpectedTransferDigests::from_hex(None, Some(&"xy".repeat(64))),
            Err(TransferDigestParseError::InvalidSha512)
        );
    }

    #[test]
    fn authenticated_contracts_require_digest_authority() {
        let one = NonZeroU64::new(1).expect("positive");
        assert_eq!(
            TransferContract::authenticated_exact(one, ExpectedTransferDigests::none()),
            Err(TransferContractError::MissingDigest)
        );
        assert_eq!(
            TransferContract::authenticated_below(one, ExpectedTransferDigests::none()),
            Err(TransferContractError::MissingDigest)
        );
        assert_eq!(
            TransferContract::unauthenticated_at_most(one).bytes(),
            TransferByteContract::AtMost(one)
        );
    }

    #[test]
    fn byte_contracts_keep_exact_at_most_and_below_distinct() {
        let four = NonZeroU64::new(4).expect("positive");
        assert!(TransferByteContract::Exact(four).admits_partial(3));
        assert!(TransferByteContract::Exact(four).admits_final(4));
        assert!(!TransferByteContract::Exact(four).admits_final(3));
        assert!(TransferByteContract::AtMost(four).admits_final(4));
        assert!(!TransferByteContract::AtMost(four).admits_final(5));
        assert!(TransferByteContract::Below(four).admits_final(3));
        assert!(!TransferByteContract::Below(four).admits_partial(4));
        assert_eq!(
            admit_bytes(TransferByteContract::Below(four), 3, 1),
            Err(TransferFailureKind::ByteLimitExceeded {
                limit: 4,
                observed: 4,
            })
        );
        assert_eq!(
            admit_bytes(TransferByteContract::AtMost(four), u64::MAX, 1),
            Err(TransferFailureKind::ByteCountOverflow)
        );
    }

    #[test]
    fn retry_policy_caps_total_attempts_at_eight() {
        fn retry_network(failure: &TransferFailureKind) -> bool {
            matches!(failure, TransferFailureKind::Network)
        }
        let seven = [Duration::from_millis(1); 7];
        let eight = [Duration::from_millis(1); 8];
        let accepted = RetryPolicy::classified(&seven, retry_network).expect("eight attempts");
        assert_eq!(accepted.delay_after(6), Some(Duration::from_millis(1)));
        assert_eq!(accepted.delay_after(7), None);
        assert!(matches!(
            RetryPolicy::classified(&eight, retry_network),
            Err(RetryPolicyError::TooManyAttempts)
        ));
        assert!(matches!(
            RetryPolicy::classified(&[Duration::ZERO], retry_network),
            Err(RetryPolicyError::ZeroDelay)
        ));
        assert!(matches!(
            RetryPolicy::classified(
                &[MAX_RETRY_DELAY + Duration::from_nanos(1)],
                retry_network,
            ),
            Err(RetryPolicyError::DelayExceedsMaximum)
        ));
        assert!(RetryPolicy::classified(&[MAX_RETRY_DELAY; 4], retry_network).is_ok());
        assert!(matches!(
            RetryPolicy::classified(&[MAX_RETRY_DELAY; 5], retry_network),
            Err(RetryPolicyError::RetryWindowExceedsMaximum)
        ));
    }

    #[test]
    fn transfer_client_config_requires_positive_bounded_timeouts() {
        let connect = Duration::from_secs(5);
        let idle_read = Duration::from_secs(30);
        let request = Duration::from_secs(60);
        let github = transfer_origin("https://github.com/owner/release");
        let release_assets = transfer_origin("https://release-assets.githubusercontent.com/file");
        let origins = vec![github.clone(), release_assets.clone()];
        let bounded = |connect, idle_read, request| {
            TransferClientConfig::bounded(connect, idle_read, request, origins.clone())
        };
        let config = bounded(connect, idle_read, request).expect("bounded transport config");
        assert_eq!(config.connect_timeout(), connect);
        assert_eq!(config.idle_read_timeout(), idle_read);
        assert_eq!(config.request_timeout(), request);
        assert_eq!(config.origin_count(), 2);
        let client = TransferClient::build(config).expect("closed transfer client");
        assert!(client.admits_url(
            &reqwest::Url::parse("https://github.com/other/path").expect("GitHub URL")
        ));
        assert!(client.admits_url(
            &reqwest::Url::parse("https://release-assets.githubusercontent.com/asset")
                .expect("release asset URL")
        ));
        assert!(!client.admits_url(
            &reqwest::Url::parse("https://example.com/asset").expect("other origin")
        ));
        assert!(!client.admits_url(
            &reqwest::Url::parse("http://github.com/downgrade").expect("downgrade URL")
        ));

        for (kind, result) in [
            (
                TransferTimeoutKind::Connect,
                bounded(Duration::ZERO, idle_read, request),
            ),
            (
                TransferTimeoutKind::IdleRead,
                bounded(connect, Duration::ZERO, request),
            ),
            (
                TransferTimeoutKind::Request,
                bounded(connect, idle_read, Duration::ZERO),
            ),
        ] {
            assert_eq!(
                result,
                Err(TransferClientConfigError::ZeroTimeout(kind))
            );
        }
        for (kind, result) in [
            (
                TransferTimeoutKind::Connect,
                bounded(
                    MAX_CONNECT_TIMEOUT + Duration::from_nanos(1),
                    idle_read,
                    MAX_REQUEST_TIMEOUT,
                ),
            ),
            (
                TransferTimeoutKind::IdleRead,
                bounded(
                    connect,
                    MAX_IDLE_READ_TIMEOUT + Duration::from_nanos(1),
                    MAX_REQUEST_TIMEOUT,
                ),
            ),
            (
                TransferTimeoutKind::Request,
                bounded(
                    connect,
                    idle_read,
                    MAX_REQUEST_TIMEOUT + Duration::from_nanos(1),
                ),
            ),
        ] {
            assert_eq!(
                result,
                Err(TransferClientConfigError::TimeoutExceedsMaximum(kind))
            );
        }
        assert_eq!(
            bounded(
                Duration::from_secs(2),
                Duration::from_secs(1),
                Duration::from_secs(1),
            ),
            Err(TransferClientConfigError::TimeoutExceedsRequest(
                TransferTimeoutKind::Connect,
            ))
        );
        assert_eq!(
            bounded(
                Duration::from_secs(1),
                Duration::from_secs(2),
                Duration::from_secs(1),
            ),
            Err(TransferClientConfigError::TimeoutExceedsRequest(
                TransferTimeoutKind::IdleRead,
            ))
        );
        assert_eq!(
            TransferClientConfig::bounded(connect, idle_read, request, Vec::new()),
            Err(TransferClientConfigError::MissingOrigins)
        );
        assert_eq!(
            TransferClientConfig::bounded(
                connect,
                idle_read,
                request,
                vec![github.clone(); MAX_TRANSFER_ORIGINS + 1],
            ),
            Err(TransferClientConfigError::TooManyOrigins)
        );
        assert_eq!(
            TransferClientConfig::bounded(
                connect,
                idle_read,
                request,
                vec![github.clone(), github],
            ),
            Err(TransferClientConfigError::DuplicateOrigin)
        );
        assert_eq!(
            TransferOrigin::from_url(
                &reqwest::Url::parse("https://user@example.com/file").expect("userinfo URL")
            ),
            Err(TransferOriginError::UserInfo)
        );
        assert_eq!(
            TransferOrigin::from_url(
                &reqwest::Url::parse("ftp://example.com/file").expect("FTP URL")
            ),
            Err(TransferOriginError::UnsupportedScheme)
        );
        let loopback_http = reqwest::Url::parse("http://127.0.0.1:8080/file")
            .expect("loopback HTTP URL");
        assert_eq!(
            TransferOrigin::from_url(&loopback_http),
            Err(TransferOriginError::UnsupportedScheme)
        );
        assert!(TransferOrigin::from_loopback_http_for_test(&loopback_http).is_ok());
        assert_eq!(
            TransferOrigin::from_loopback_http_for_test(
                &reqwest::Url::parse("http://192.0.2.1/file").expect("remote HTTP URL")
            ),
            Err(TransferOriginError::UnsupportedScheme)
        );
    }

    #[test]
    fn engine_retry_ceiling_allows_only_documented_transients() {
        for failure in [
            TransferFailureKind::Network,
            TransferFailureKind::ProviderStatus(408),
            TransferFailureKind::ProviderStatus(425),
            TransferFailureKind::ProviderStatus(429),
            TransferFailureKind::ProviderStatus(500),
            TransferFailureKind::ProviderStatus(599),
        ] {
            assert!(failure.is_policy_retryable(), "{failure:?}");
        }
        for failure in [
            TransferFailureKind::RequestPolicy,
            TransferFailureKind::ProviderStatus(301),
            TransferFailureKind::ProviderStatus(404),
            TransferFailureKind::ProviderStatus(409),
            TransferFailureKind::ProviderStatus(600),
        ] {
            assert!(!failure.is_policy_retryable(), "{failure:?}");
        }
    }

    #[test]
    fn cleanup_pending_preserves_the_actual_producer_cause() {
        assert_eq!(
            select_attempt_failure(
                ProducerExit::Failed(TransferFailureKind::Network),
                TransferFailureKind::Cancelled,
            ),
            TransferFailureKind::Network
        );
        assert_eq!(
            select_attempt_failure(
                ProducerExit::Failed(TransferFailureKind::Network),
                TransferFailureKind::StageWrite(io::ErrorKind::WriteZero),
            ),
            TransferFailureKind::StageWrite(io::ErrorKind::WriteZero)
        );
    }

    #[test]
    fn provider_requires_exactly_ok_without_a_range_request() {
        assert_eq!(provider_status_failure(reqwest::StatusCode::OK), None);
        for status in [
            reqwest::StatusCode::CREATED,
            reqwest::StatusCode::NO_CONTENT,
            reqwest::StatusCode::PARTIAL_CONTENT,
        ] {
            assert_eq!(
                provider_status_failure(status),
                Some(TransferFailureKind::ProviderStatus(status.as_u16()))
            );
        }
    }

    #[tokio::test]
    async fn cancellation_owner_drop_wakes_waiters() {
        let (sender, mut cancellation) = transfer_cancellation_channel();
        drop(sender);
        cancellation.cancelled().await;
        assert!(cancellation.is_cancelled());
    }

    #[tokio::test]
    async fn writer_channel_closure_interrupts_a_pending_provider_wait() {
        let (owner, mut cancellation) = transfer_cancellation_channel();
        let (messages, receiver) = tokio::sync::mpsc::channel::<WriterMessage>(1);
        let wait = wait_for_writer(
            &mut cancellation,
            &messages,
            std::future::pending::<()>(),
        );
        let close_writer = async move {
            tokio::task::yield_now().await;
            drop(receiver);
        };
        let (result, ()) = tokio::join!(wait, close_writer);
        assert_eq!(result, None);
        assert_eq!(
            wait_interruption(&cancellation, &messages),
            TransferFailureKind::ChannelClosed
        );
        drop(owner);
    }

    #[tokio::test]
    async fn transfer_task_drop_cancels_its_supervisor() {
        let (sender, cancellation) = transfer_cancellation_channel();
        let task_cancellation = Arc::clone(&cancellation.shared);
        let mut supervisor_cancellation = cancellation.clone();
        let (finished, finished_rx) = tokio::sync::oneshot::channel();
        let join = tokio::spawn(async move {
            supervisor_cancellation.cancelled().await;
            let _ = finished.send(());
            TransferOutcome::Failed(TransferFailureReport::single(
                TransferFailureKind::Cancelled,
            ))
        });
        let task = TransferTask::<()> {
            cancellation: task_cancellation,
            join: Some(join),
        };
        drop(task);
        finished_rx.await.expect("supervisor observed task drop");
        assert!(cancellation.is_cancelled());
        drop(sender);
    }

    #[test]
    fn content_encoding_accepts_only_absent_or_identity_values() {
        use reqwest::header::{HeaderMap, HeaderValue};

        let mut headers = HeaderMap::new();
        assert!(headers_have_identity_encoding(&headers));
        headers.insert(CONTENT_ENCODING, HeaderValue::from_static("identity"));
        assert!(headers_have_identity_encoding(&headers));
        headers.insert(CONTENT_ENCODING, HeaderValue::from_static("IDENTITY"));
        assert!(headers_have_identity_encoding(&headers));
        headers.insert(
            CONTENT_ENCODING,
            HeaderValue::from_static("identity, identity"),
        );
        assert!(headers_have_identity_encoding(&headers));
        headers.insert(CONTENT_ENCODING, HeaderValue::from_static("gzip"));
        assert!(!headers_have_identity_encoding(&headers));
        headers.insert(CONTENT_ENCODING, HeaderValue::from_static("br"));
        assert!(!headers_have_identity_encoding(&headers));
        headers.insert(CONTENT_ENCODING, HeaderValue::from_static("deflate"));
        assert!(!headers_have_identity_encoding(&headers));
        headers.insert(CONTENT_ENCODING, HeaderValue::from_static("zstd"));
        assert!(!headers_have_identity_encoding(&headers));
        headers.insert(
            CONTENT_ENCODING,
            HeaderValue::from_static("identity, gzip"),
        );
        assert!(!headers_have_identity_encoding(&headers));
        headers.insert(CONTENT_ENCODING, HeaderValue::from_static(""));
        assert!(!headers_have_identity_encoding(&headers));
    }

    #[test]
    fn requested_digest_combinations_hash_only_requested_algorithms() {
        const BODY: &[u8] = b"digest combinations";
        let none = WriterHashers::new(&ExpectedTransferDigests::none()).finish();
        assert_eq!(none.sha1(), None);
        assert_eq!(none.sha512(), None);

        let mut sha1 = WriterHashers::new(&ExpectedTransferDigests::sha1([0; 20]));
        sha1.update(BODY);
        let sha1 = sha1.finish();
        assert_eq!(sha1.sha1(), Some(&Sha1::digest(BODY).into()));
        assert_eq!(sha1.sha512(), None);

        let mut sha512 = WriterHashers::new(&ExpectedTransferDigests::sha512([0; 64]));
        sha512.update(BODY);
        let sha512 = sha512.finish();
        assert_eq!(sha512.sha1(), None);
        assert_eq!(sha512.sha512(), Some(&Sha512::digest(BODY).into()));

        let mut both = WriterHashers::new(&ExpectedTransferDigests::both([0; 20], [0; 64]));
        both.update(BODY);
        let both = both.finish();
        assert!(both.sha1().is_some());
        assert!(both.sha512().is_some());
        assert_eq!(
            format!("{both:?}"),
            "VerifiedTransferDigests { sha1: true, sha512: true }"
        );
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn source_transfer_verifies_replays_and_discards_without_publication() {
        const BODY: &[u8] = b"bounded managed source";
        let (url, server) = serve_once(BODY).await;
        let temporary = tempfile::tempdir().expect("temporary root");
        let session = acquire_test_root(temporary.path());
        let root = session.root().expect("root capability");
        let expected_sha1: [u8; 20] = Sha1::digest(BODY).into();
        let contract = TransferContract::authenticated_exact(
            NonZeroU64::new(BODY.len() as u64).expect("non-empty body"),
            ExpectedTransferDigests::sha1(expected_sha1),
        )
        .expect("authenticated contract");
        let (cancellation_owner, cancellation) = transfer_cancellation_channel();
        let destination = root
            .admit_transient_destination(
                LeafName::new("source-reservation").expect("portable reservation"),
            )
            .expect("reserve source destination");
        let task = start_source_transfer(
            test_transfer_client(&url),
            url,
            SourceOnlyTransferTarget::new(destination),
            contract,
            RetryPolicy::none(),
            cancellation,
        );
        let mut source = match task.join().await {
            TransferOutcome::Complete(source) => source,
            outcome => panic!("source transfer failed: {outcome:?}"),
        };
        assert_eq!(source.report().bytes(), BODY.len() as u64);
        assert_eq!(source.report().declared_length(), Some(BODY.len() as u64));
        let mut first = Vec::new();
        source.read_to_end(&mut first).expect("first read");
        assert_eq!(first, BODY);
        source.seek(SeekFrom::Start(0)).expect("rewind");
        let mut second = Vec::new();
        source.read_to_end(&mut second).expect("second read");
        assert_eq!(second, BODY);
        assert!(matches!(
            source.discard(),
            VerifiedTransferDiscardOutcome::Discarded(_)
        ));
        server.await.expect("server task");
        drop(cancellation_owner);
        drop(root);
        assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn create_only_transfer_publishes_through_singleton_batch() {
        const BODY: &[u8] = b"bounded managed publication";
        let (url, server) = serve_once(BODY).await;
        let temporary = tempfile::tempdir().expect("temporary root");
        let session = acquire_test_root(temporary.path());
        let root = session.root().expect("root capability");
        let leaf = LeafName::new("published-artifact").expect("portable destination");
        let expected_sha1: [u8; 20] = Sha1::digest(BODY).into();
        let contract = TransferContract::authenticated_exact(
            NonZeroU64::new(BODY.len() as u64).expect("non-empty body"),
            ExpectedTransferDigests::sha1(expected_sha1),
        )
        .expect("authenticated contract");
        let (cancellation_owner, cancellation) = transfer_cancellation_channel();
        let destination = root
            .admit_transient_destination(leaf)
            .expect("reserve publication destination");
        let task = start_create_only_transfer(
            test_transfer_client(&url),
            url,
            CreateOnlyTransferTarget::new(destination),
            contract,
            RetryPolicy::none(),
            cancellation,
        );
        let verified = match task.join().await {
            TransferOutcome::Complete(verified) => verified,
            outcome => panic!("create-only transfer failed: {outcome:?}"),
        };
        let (file, report) = match verified.publish_create_new() {
            TransferPublicationOutcome::Published { file, report } => (file, report),
            outcome => panic!("create-only publication did not settle: {outcome:?}"),
        };
        assert_eq!(report.bytes(), BODY.len() as u64);
        assert_eq!(
            std::fs::read(temporary.path().join("published-artifact"))
                .expect("read published artifact"),
            BODY
        );
        server.await.expect("server task");
        drop(file);
        drop(cancellation_owner);
        drop(root);
        assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn authenticated_terminal_failure_discards_stage_and_cancels_destination() {
        const BODY: &[u8] = b"authenticated terminal failure";
        let (url, server) = serve_once(BODY).await;
        let temporary = tempfile::tempdir().expect("temporary root");
        let session = acquire_test_root(temporary.path());
        let root = session.root().expect("root capability");
        let leaf = LeafName::new("failed-reservation").expect("portable reservation");
        let contract = TransferContract::authenticated_exact(
            NonZeroU64::new(BODY.len() as u64).expect("non-empty body"),
            ExpectedTransferDigests::sha1([0; 20]),
        )
        .expect("authenticated contract");
        let (cancellation_owner, cancellation) = transfer_cancellation_channel();
        let destination = root
            .admit_transient_destination(leaf)
            .expect("reserve failed destination");
        let task = start_create_only_transfer(
            test_transfer_client(&url),
            url,
            CreateOnlyTransferTarget::new(destination),
            contract,
            RetryPolicy::none(),
            cancellation,
        );
        let report = match task.join().await {
            TransferOutcome::Failed(report) => report,
            outcome => panic!("terminal transfer did not settle: {outcome:?}"),
        };
        assert_eq!(report.attempts(), 1);
        assert_eq!(
            report.last(),
            TransferFailureKind::DigestMismatch(TransferDigestAlgorithm::Sha1)
        );
        assert!(!temporary.path().join("failed-reservation").exists());
        server.await.expect("server task");
        drop(cancellation_owner);
        drop(root);
        assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
    }

    fn transfer_origin(url: &str) -> TransferOrigin {
        let url = reqwest::Url::parse(url).expect("origin URL");
        TransferOrigin::from_url(&url).expect("transfer origin")
    }

    fn test_transfer_client(url: &reqwest::Url) -> TransferClient {
        let config = TransferClientConfig::bounded(
            Duration::from_secs(5),
            Duration::from_secs(5),
            Duration::from_secs(10),
            vec![
                TransferOrigin::from_loopback_http_for_test(url)
                    .expect("loopback test server origin"),
            ],
        )
        .expect("bounded test transport");
        TransferClient::build(config).expect("transfer client")
    }

    fn acquire_test_root(path: &std::path::Path) -> RootSession {
        match RootSession::acquire(path) {
            RootSessionAcquireOutcome::Acquired(session) => session,
            RootSessionAcquireOutcome::NoEffect(error) => {
                panic!("root acquisition had no effect: {error}")
            }
            RootSessionAcquireOutcome::AppliedUnverified(obligation) => {
                match obligation.reconcile() {
                    RootSessionAcquireOutcome::Acquired(session) => session,
                    RootSessionAcquireOutcome::NoEffect(error) => {
                        panic!("root acquisition reconciliation had no effect: {error}")
                    }
                    RootSessionAcquireOutcome::AppliedUnverified(obligation) => {
                        let error = obligation.error().to_string();
                        let _ = obligation.cleanup();
                        panic!("root acquisition remained indeterminate: {error}")
                    }
                }
            }
        }
    }

    async fn serve_once(body: &'static [u8]) -> (reqwest::Url, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind transfer fixture");
        let address = listener.local_addr().expect("fixture address");
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept transfer request");
            let mut request = [0_u8; 2048];
            let read = stream.read(&mut request).await.expect("read request");
            let request = std::str::from_utf8(&request[..read]).expect("HTTP request text");
            assert!(request.to_ascii_lowercase().contains("accept-encoding: identity"));
            let headers = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            stream
                .write_all(headers.as_bytes())
                .await
                .expect("write response headers");
            stream.write_all(body).await.expect("write response body");
        });
        (
            reqwest::Url::parse(&format!("http://{address}/artifact"))
                .expect("fixture URL"),
            server,
        )
    }
}
