use std::io::{Read, Write};

use reqwest::header::{
    ACCEPT_ENCODING, CONTENT_ENCODING, CONTENT_LENGTH, CONTENT_RANGE, CONTENT_TYPE, LOCATION,
    TRANSFER_ENCODING,
};
use reqwest::{Response, StatusCode};
use sha1::Sha1;
use sha2::{Digest, Sha256, Sha512};

use crate::tasks::CancellationToken;

use super::policy::{
    ClientConfig, DownloadError, DownloadRequest, HashAlgorithm, IntegrityEvidence, IntegrityPolicy,
};

#[derive(Clone)]
pub struct ProviderClient {
    client: reqwest::Client,
    config: ClientConfig,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DownloadEvidence {
    pub encoded_bytes: u64,
    pub decoded_bytes: u64,
    pub observed_sha256: String,
    pub integrity: IntegrityEvidence,
    pub redirects: usize,
    /// Deliberately excludes path and query, which may contain provider secrets.
    pub final_origin: String,
}

/// Cannot be constructed by consumers from an interrupted or unchecked response.
pub struct DownloadedBytes {
    bytes: Vec<u8>,
    evidence: DownloadEvidence,
}

impl DownloadedBytes {
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }

    pub fn evidence(&self) -> &DownloadEvidence {
        &self.evidence
    }
}

impl std::fmt::Debug for DownloadedBytes {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DownloadedBytes")
            .field("evidence", &self.evidence)
            .finish_non_exhaustive()
    }
}

impl ProviderClient {
    pub fn new(config: ClientConfig) -> Result<Self, DownloadError> {
        if config.connect_timeout.is_zero()
            || config.read_timeout.is_zero()
            || config.total_timeout.is_zero()
            || config.user_agent.is_empty()
            || config.user_agent.len() > 256
        {
            return Err(DownloadError::InvalidPolicy);
        }
        let client = Self::builder(&config)
            .build()
            .map_err(|_| DownloadError::InvalidPolicy)?;
        Ok(Self { client, config })
    }

    fn builder(config: &ClientConfig) -> reqwest::ClientBuilder {
        reqwest::Client::builder()
            .user_agent(config.user_agent.as_str())
            .connect_timeout(config.connect_timeout)
            .read_timeout(config.read_timeout)
            .pool_max_idle_per_host(32)
            .pool_idle_timeout(std::time::Duration::from_secs(120))
            .tcp_keepalive(std::time::Duration::from_secs(60))
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            // Count HTTP representation bytes before any decoder changes them.
            .no_gzip()
            .no_brotli()
            .no_deflate()
            .no_zstd()
            .referer(false)
    }

    /// Does not retry: the domain decides whether and when to repeat a query.
    /// Cancellation drops the live HTTP/decompression work; no background task or
    /// filesystem effect is created, and no partially filled buffer is returned.
    pub async fn fetch(
        &self,
        request: DownloadRequest,
        cancellation: &CancellationToken,
    ) -> Result<DownloadedBytes, DownloadError> {
        self.fetch_with_clients(request, cancellation, |_| {
            std::future::ready(Ok(self.client.clone()))
        })
        .await
    }

    /// Like `fetch`, but every admitted request origin is resolved to public IPs
    /// and pinned before connecting. Proxies cannot bypass that address check.
    /// The caller's existing origin/redirect policy is preserved, including
    /// explicitly allowed cross-origin GET redirects. DNS is inside the same
    /// cancellation scope and total deadline as decoding and integrity checks.
    pub async fn fetch_public(
        &self,
        request: DownloadRequest,
        cancellation: &CancellationToken,
    ) -> Result<DownloadedBytes, DownloadError> {
        self.fetch_public_resolved(request, cancellation, super::managed::lookup_candidates)
            .await
    }

    pub(super) async fn fetch_public_resolved<R, F>(
        &self,
        request: DownloadRequest,
        cancellation: &CancellationToken,
        resolve: R,
    ) -> Result<DownloadedBytes, DownloadError>
    where
        R: Fn(String, u16) -> F,
        F: std::future::Future<Output = std::io::Result<Vec<std::net::SocketAddr>>>,
    {
        self.fetch_with_clients(request, cancellation, |url| {
            let resolve = &resolve;
            async move {
                let addresses = super::managed::public_addresses(&url, resolve)
                    .await
                    .map_err(|error| match error.kind() {
                        std::io::ErrorKind::InvalidInput => DownloadError::OriginNotAllowed,
                        std::io::ErrorKind::TimedOut => DownloadError::Timeout,
                        _ => DownloadError::Network,
                    })?;
                Self::builder(&self.config)
                    .no_proxy()
                    .resolve_to_addrs(url.host_str().ok_or(DownloadError::InvalidUrl)?, &addresses)
                    .build()
                    .map_err(|_| DownloadError::InvalidPolicy)
            }
        })
        .await
    }

    async fn fetch_with_clients<C, F>(
        &self,
        request: DownloadRequest,
        cancellation: &CancellationToken,
        clients: C,
    ) -> Result<DownloadedBytes, DownloadError>
    where
        C: Fn(reqwest::Url) -> F,
        F: std::future::Future<Output = Result<reqwest::Client, DownloadError>>,
    {
        tokio::select! {
            biased;
            _ = cancellation.cancelled() => Err(DownloadError::Cancelled),
            result = tokio::time::timeout(self.config.total_timeout, self.fetch_inner(request, cancellation, clients)) => {
                result.map_err(|_| DownloadError::Timeout)?
            }
        }
    }

    async fn fetch_inner<C, F>(
        &self,
        request: DownloadRequest,
        cancellation: &CancellationToken,
        clients: C,
    ) -> Result<DownloadedBytes, DownloadError>
    where
        C: Fn(reqwest::Url) -> F,
        F: std::future::Future<Output = Result<reqwest::Client, DownloadError>>,
    {
        request.limits.validate()?;
        if request
            .integrity
            .expected_size()
            .is_some_and(|size| size > request.limits.max_decoded_bytes)
        {
            return Err(DownloadError::InvalidPolicy);
        }
        let mut url = request.origins.admit(&request.url)?;
        let request_body = request.body.as_ref().map(encode_query).transpose()?;
        let mut redirects = 0;
        let response = loop {
            check_cancelled(cancellation)?;
            let client = clients(url.clone()).await?;
            let builder = if let Some(body) = &request_body {
                client
                    .post(url.clone())
                    .header(CONTENT_TYPE, "application/json")
                    .body(body.clone())
            } else {
                client.get(url.clone())
            };
            let response = builder
                .header(ACCEPT_ENCODING, "gzip, deflate")
                .send()
                .await
                .map_err(request_error)?;
            if !matches!(response.status().as_u16(), 301 | 302 | 303 | 307 | 308) {
                break response;
            }
            if redirects >= request.origins.max_redirects {
                return Err(DownloadError::RedirectLimit);
            }
            let mut locations = response.headers().get_all(LOCATION).iter();
            let location = locations
                .next()
                .and_then(|value| value.to_str().ok())
                .filter(|value| !value.is_empty())
                .ok_or(DownloadError::InvalidRedirect)?;
            if locations.next().is_some() {
                return Err(DownloadError::InvalidRedirect);
            }
            let next = url
                .join(location)
                .map_err(|_| DownloadError::InvalidRedirect)?;
            let next = request.origins.admit(next.as_str())?;
            if request_body.is_some()
                && (next.origin() != url.origin()
                    || !matches!(response.status().as_u16(), 307 | 308))
            {
                return Err(DownloadError::RedirectMethod);
            }
            url = next;
            redirects += 1;
        };

        if response.status() == StatusCode::PARTIAL_CONTENT
            || response.headers().contains_key(CONTENT_RANGE)
        {
            return Err(DownloadError::IncompleteBody);
        }
        if !response.status().is_success() {
            return Err(DownloadError::HttpStatus {
                status: response.status().as_u16(),
            });
        }
        let encoding = response_encoding(&response)?;
        let body = read_response(response, request.limits.max_encoded_bytes, cancellation).await?;
        let encoded_bytes = body.len() as u64;
        let bytes = decode_body(
            body,
            encoding,
            request.limits.max_decoded_bytes,
            cancellation,
        )
        .await?;
        let (integrity, observed_sha256) = verify(&bytes, request.integrity, cancellation).await?;
        check_cancelled(cancellation)?;
        Ok(DownloadedBytes {
            evidence: DownloadEvidence {
                encoded_bytes,
                decoded_bytes: bytes.len() as u64,
                observed_sha256,
                integrity,
                redirects,
                final_origin: url.origin().ascii_serialization(),
            },
            bytes,
        })
    }
}

fn request_error(error: reqwest::Error) -> DownloadError {
    if error.is_timeout() {
        DownloadError::Timeout
    } else if error.is_body() || error.is_decode() {
        DownloadError::IncompleteBody
    } else {
        DownloadError::Network
    }
}

fn check_cancelled(cancellation: &CancellationToken) -> Result<(), DownloadError> {
    if cancellation.is_cancelled() {
        Err(DownloadError::Cancelled)
    } else {
        Ok(())
    }
}

async fn read_response(
    mut response: Response,
    limit: u64,
    cancellation: &CancellationToken,
) -> Result<Vec<u8>, DownloadError> {
    let mut lengths = response.headers().get_all(CONTENT_LENGTH).iter();
    let declared = lengths
        .next()
        .map(|value| {
            let value = value.to_str().map_err(|_| DownloadError::IncompleteBody)?;
            if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
                return Err(DownloadError::IncompleteBody);
            }
            value
                .parse::<u64>()
                .map_err(|_| DownloadError::IncompleteBody)
        })
        .transpose()?;
    if lengths.next().is_some()
        || (declared.is_some() && response.headers().contains_key(TRANSFER_ENCODING))
    {
        return Err(DownloadError::IncompleteBody);
    }
    if declared.is_some_and(|length| length > limit) {
        return Err(DownloadError::EncodedLimitExceeded { limit });
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(request_error)? {
        check_cancelled(cancellation)?;
        let next = (bytes.len() as u64)
            .checked_add(chunk.len() as u64)
            .ok_or(DownloadError::EncodedLimitExceeded { limit })?;
        if next > limit {
            return Err(DownloadError::EncodedLimitExceeded { limit });
        }
        bytes
            .try_reserve(chunk.len())
            .map_err(|_| DownloadError::AllocationFailed)?;
        bytes.extend_from_slice(&chunk);
        tokio::task::yield_now().await;
    }
    if declared.is_some_and(|length| length != bytes.len() as u64) {
        return Err(DownloadError::IncompleteBody);
    }
    Ok(bytes)
}

#[derive(Clone, Copy)]
enum Encoding {
    Identity,
    Gzip,
    Deflate,
}

fn response_encoding(response: &Response) -> Result<Encoding, DownloadError> {
    let mut encodings = response.headers().get_all(CONTENT_ENCODING).iter();
    let encoding = match encodings.next() {
        None => Encoding::Identity,
        Some(value) => match value
            .to_str()
            .map_err(|_| DownloadError::UnsupportedEncoding)?
            .trim()
            .to_ascii_lowercase()
            .as_str()
        {
            "identity" => Encoding::Identity,
            "gzip" => Encoding::Gzip,
            "deflate" => Encoding::Deflate,
            _ => return Err(DownloadError::UnsupportedEncoding),
        },
    };
    if encodings.next().is_some() {
        return Err(DownloadError::UnsupportedEncoding);
    }
    Ok(encoding)
}

async fn decode_body(
    body: Vec<u8>,
    encoding: Encoding,
    limit: u64,
    cancellation: &CancellationToken,
) -> Result<Vec<u8>, DownloadError> {
    if matches!(encoding, Encoding::Identity) {
        if body.len() as u64 > limit {
            return Err(DownloadError::DecodedLimitExceeded { limit });
        }
        return Ok(body);
    }
    match encoding {
        Encoding::Gzip => {
            // Multi-member decoding prevents appended gzip members bypassing the
            // decoded limit or the checksum of the complete representation.
            let mut decoder = flate2::bufread::MultiGzDecoder::new(body.as_slice());
            decode_reader(&mut decoder, limit, cancellation).await
        }
        Encoding::Deflate => decode_zlib(&body, limit, cancellation).await,
        Encoding::Identity => unreachable!("identity handled before decoder construction"),
    }
}

async fn decode_zlib(
    body: &[u8],
    limit: u64,
    cancellation: &CancellationToken,
) -> Result<Vec<u8>, DownloadError> {
    let mut decoder = flate2::Decompress::new(true);
    let mut bytes = Vec::new();
    let mut buffer = vec![0_u8; 64 * 1024];
    loop {
        check_cancelled(cancellation)?;
        let previous_in = decoder.total_in();
        let previous_out = decoder.total_out();
        let status = decoder
            .decompress(
                &body[previous_in as usize..],
                &mut buffer,
                // Output is deliberately chunked. Finish requires enough output
                // space for the entire remaining stream, which this buffer does
                // not promise. StreamEnd still verifies the complete trailer.
                flate2::FlushDecompress::None,
            )
            .map_err(|_| DownloadError::InvalidEncoding)?;
        let read = (decoder.total_out() - previous_out) as usize;
        if (bytes.len() as u64).saturating_add(read as u64) > limit {
            return Err(DownloadError::DecodedLimitExceeded { limit });
        }
        bytes
            .try_reserve(read)
            .map_err(|_| DownloadError::AllocationFailed)?;
        bytes.extend_from_slice(&buffer[..read]);
        if status == flate2::Status::StreamEnd {
            if decoder.total_in() as usize != body.len() {
                return Err(DownloadError::InvalidEncoding);
            }
            return Ok(bytes);
        }
        // Read adapters may return EOF on an incomplete zlib stream. Require the
        // codec's verified end marker, including its checksum trailer.
        if previous_in == decoder.total_in() && read == 0 {
            return Err(DownloadError::InvalidEncoding);
        }
        tokio::task::yield_now().await;
    }
}

async fn decode_reader(
    reader: &mut impl Read,
    limit: u64,
    cancellation: &CancellationToken,
) -> Result<Vec<u8>, DownloadError> {
    let mut bytes = Vec::new();
    let mut buffer = vec![0_u8; 64 * 1024];
    loop {
        check_cancelled(cancellation)?;
        let read = reader
            .read(&mut buffer)
            .map_err(|_| DownloadError::InvalidEncoding)?;
        if read == 0 {
            return Ok(bytes);
        }
        if (bytes.len() as u64).saturating_add(read as u64) > limit {
            return Err(DownloadError::DecodedLimitExceeded { limit });
        }
        bytes
            .try_reserve(read)
            .map_err(|_| DownloadError::AllocationFailed)?;
        bytes.extend_from_slice(&buffer[..read]);
        // Bounded chunks yield during CPU work so timeout/cancellation are not
        // postponed until a large artifact has finished decoding.
        tokio::task::yield_now().await;
    }
}

async fn verify(
    bytes: &[u8],
    policy: IntegrityPolicy,
    cancellation: &CancellationToken,
) -> Result<(IntegrityEvidence, String), DownloadError> {
    if policy
        .expected_size()
        .is_some_and(|size| size != bytes.len() as u64)
    {
        return Err(DownloadError::SizeMismatch);
    }
    let mut sha256 = Sha256::new();
    let mut sha1 = matches!(&policy, IntegrityPolicy::Checksum { checksum, .. } if checksum.algorithm == HashAlgorithm::Sha1)
        .then(Sha1::new);
    let mut sha512 = matches!(&policy, IntegrityPolicy::Checksum { checksum, .. } if checksum.algorithm == HashAlgorithm::Sha512)
        .then(Sha512::new);
    for chunk in bytes.chunks(64 * 1024) {
        check_cancelled(cancellation)?;
        sha256.update(chunk);
        if let Some(hash) = &mut sha1 {
            hash.update(chunk);
        }
        if let Some(hash) = &mut sha512 {
            hash.update(chunk);
        }
        tokio::task::yield_now().await;
    }
    let sha256 = sha256.finalize().to_vec();
    let integrity = match policy {
        IntegrityPolicy::Checksum { checksum, .. } => {
            let actual = match checksum.algorithm {
                HashAlgorithm::Sha1 => sha1
                    .expect("SHA1 policy selected the hasher")
                    .finalize()
                    .to_vec(),
                HashAlgorithm::Sha256 => sha256.clone(),
                HashAlgorithm::Sha512 => sha512
                    .expect("SHA512 policy selected the hasher")
                    .finalize()
                    .to_vec(),
            };
            if actual != checksum.bytes {
                return Err(DownloadError::ChecksumMismatch);
            }
            IntegrityEvidence::ChecksumMatched {
                algorithm: checksum.algorithm,
            }
        }
        IntegrityPolicy::Unhashed { reason, .. } => IntegrityEvidence::Unhashed { reason },
    };
    Ok((integrity, hex::encode(sha256)))
}

fn encode_query(value: &serde_json::Value) -> Result<Vec<u8>, DownloadError> {
    struct Query(Vec<u8>);
    impl Write for Query {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if self.0.len().saturating_add(bytes.len()) > 1024 * 1024 {
                return Err(std::io::Error::other("query exceeds byte limit"));
            }
            self.0.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut query = Query(Vec::new());
    serde_json::to_writer(&mut query, value).map_err(|_| DownloadError::RequestBodyTooLarge)?;
    Ok(query.0)
}

#[cfg(test)]
mod cancellation_tests {
    use std::future::{Future, poll_fn};
    use std::task::Poll;

    use super::*;
    use crate::network::UnhashedReason;

    #[tokio::test]
    async fn cancellation_between_decoder_chunks_discards_both_compression_formats() {
        let payload = vec![b'x'; 160 * 1024];
        let mut gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        gzip.write_all(&payload).unwrap();
        let mut deflate = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
        deflate.write_all(&payload).unwrap();
        for (encoding, bytes) in [
            (Encoding::Gzip, gzip.finish().unwrap()),
            (Encoding::Deflate, deflate.finish().unwrap()),
        ] {
            let token = CancellationToken::new();
            let mut decoding = Box::pin(decode_body(bytes, encoding, 1024 * 1024, &token));
            // Poll until the decoder's first cooperative yield; cancellation is
            // now deterministically after real decompression work has started.
            poll_fn(|context| {
                assert!(decoding.as_mut().poll(context).is_pending());
                Poll::Ready(())
            })
            .await;
            token.cancel();
            assert_eq!(decoding.await, Err(DownloadError::Cancelled));
        }
    }

    #[tokio::test]
    async fn cancellation_between_hash_chunks_discards_unverified_content() {
        let payload = vec![b'x'; 160 * 1024];
        let token = CancellationToken::new();
        let mut verification = Box::pin(verify(
            &payload,
            IntegrityPolicy::Unhashed {
                reason: UnhashedReason::ProviderMetadata,
                expected_size: Some(payload.len() as u64),
            },
            &token,
        ));
        poll_fn(|context| {
            assert!(verification.as_mut().poll(context).is_pending());
            Poll::Ready(())
        })
        .await;
        token.cancel();
        assert_eq!(verification.await, Err(DownloadError::Cancelled));
    }
}
