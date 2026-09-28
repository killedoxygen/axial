//! Public DNS admission for the retained managed-file transfer leaf. This
//! adapter supplies network policy; the leaf retains staging and settlement.

use std::collections::HashSet;
use std::io;
use std::net::SocketAddr;
use std::time::Duration;

use axial_minecraft::download::{
    PinnedTransferOrigin, RetryPolicy, TransferClient, TransferClientConfig, TransferFailureKind,
    TransferOrigin,
};

const RETRY_DELAYS: [Duration; 3] = [
    Duration::from_millis(500),
    Duration::from_millis(1_500),
    Duration::from_millis(4_000),
];
const DNS_TIMEOUT: Duration = Duration::from_secs(15);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const IDLE_READ_TIMEOUT: Duration = Duration::from_secs(90);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const MAX_PINNED_ADDRESSES: usize = 32;

/// Pins the admitted origin to a bounded public address set. The retained leaf
/// disables proxies, automatic retries and content decoding for these clients,
/// and admits redirects only against the pinned origin. No transfer starts here.
pub async fn pinned_public_transfer_client(
    origin: TransferOrigin,
    url: &reqwest::Url,
) -> io::Result<TransferClient> {
    if TransferOrigin::from_url(url).map_err(|_| admission_error())? != origin
        || url.fragment().is_some()
    {
        return Err(admission_error());
    }
    let addresses = public_addresses(url, lookup_candidates).await?;
    let config = pinned_config(origin, addresses)?;
    TransferClient::build(config).map_err(|_| admission_error())
}

/// Both buffered provider reads and retained file transfers use this exact
/// public-address admission. The resolver is injected only by module tests.
pub(super) async fn public_addresses<R, F>(
    url: &reqwest::Url,
    resolve: R,
) -> io::Result<Vec<SocketAddr>>
where
    R: Fn(String, u16) -> F,
    F: std::future::Future<Output = io::Result<Vec<SocketAddr>>>,
{
    let origin = TransferOrigin::from_url(url).map_err(|_| admission_error())?;
    if url.fragment().is_some() {
        return Err(admission_error());
    }
    let port = url.port_or_known_default().ok_or_else(admission_error)?;
    let addresses = match url.host().ok_or_else(admission_error)? {
        url::Host::Ipv4(address) => vec![SocketAddr::new(address.into(), port)],
        url::Host::Ipv6(address) => vec![SocketAddr::new(address.into(), port)],
        url::Host::Domain(host) => {
            let resolved = tokio::time::timeout(DNS_TIMEOUT, resolve(host.to_owned(), port))
                .await
                .map_err(|_| {
                    io::Error::new(
                        io::ErrorKind::TimedOut,
                        "public origin resolution timed out",
                    )
                })??;
            bounded_unique_addresses(resolved)
        }
    };
    PinnedTransferOrigin::public(origin, addresses.clone()).map_err(|_| admission_error())?;
    Ok(addresses)
}

pub(super) async fn lookup_candidates(host: String, port: u16) -> io::Result<Vec<SocketAddr>> {
    Ok(bounded_unique_addresses(
        tokio::net::lookup_host((host.as_str(), port)).await?,
    ))
}

/// Retains the managed transfer's finite, transient-failure-only retry policy.
/// Digest, size, admission, cancellation and filesystem failures are not retried.
pub fn managed_transfer_retry_policy() -> RetryPolicy {
    RetryPolicy::classified(&RETRY_DELAYS, transfer_retryable)
        .expect("fixed managed transfer retry policy is valid")
}

fn pinned_config(
    origin: TransferOrigin,
    addresses: Vec<SocketAddr>,
) -> io::Result<TransferClientConfig> {
    let pinned = PinnedTransferOrigin::public(origin, addresses).map_err(|_| admission_error())?;
    TransferClientConfig::bounded_pinned_public(
        CONNECT_TIMEOUT,
        IDLE_READ_TIMEOUT,
        REQUEST_TIMEOUT,
        vec![pinned],
    )
    .map_err(|_| admission_error())
}

fn bounded_unique_addresses(addresses: impl IntoIterator<Item = SocketAddr>) -> Vec<SocketAddr> {
    let mut seen = HashSet::new();
    let mut unique = Vec::with_capacity(MAX_PINNED_ADDRESSES);
    for address in addresses {
        if seen.insert(address) {
            if unique.len() == MAX_PINNED_ADDRESSES {
                break;
            }
            unique.push(address);
        }
    }
    unique
}

fn transfer_retryable(failure: &TransferFailureKind) -> bool {
    matches!(
        failure,
        TransferFailureKind::Network | TransferFailureKind::ProviderStatus(408 | 429 | 500..=599)
    )
}

fn admission_error() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        "public transfer origin could not be pinned",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn literal_public_addresses_build_without_dns_or_a_transfer() {
        for source in [
            "https://1.1.1.1/artifact",
            "https://[2606:4700:4700::1111]/artifact",
        ] {
            let url = reqwest::Url::parse(source).unwrap();
            assert!(
                pinned_public_transfer_client(TransferOrigin::from_url(&url).unwrap(), &url)
                    .await
                    .is_ok()
            );
        }
    }

    #[tokio::test]
    async fn private_addresses_and_mismatched_origins_are_rejected_without_transfer() {
        for source in [
            "https://127.0.0.1/artifact?secret=yes",
            "https://10.1.2.3/artifact",
            "https://[::1]/artifact",
            "https://1.1.1.1/artifact#fragment",
        ] {
            let url = reqwest::Url::parse(source).unwrap();
            let error =
                pinned_public_transfer_client(TransferOrigin::from_url(&url).unwrap(), &url)
                    .await
                    .unwrap_err();
            assert_eq!(
                error.to_string(),
                "public transfer origin could not be pinned"
            );
        }
        let admitted =
            TransferOrigin::from_url(&reqwest::Url::parse("https://admitted.invalid").unwrap())
                .unwrap();
        let other = reqwest::Url::parse("https://other.invalid").unwrap();
        assert!(
            pinned_public_transfer_client(admitted, &other)
                .await
                .is_err()
        );
    }

    #[test]
    fn mixed_public_and_private_dns_answers_fail_closed() {
        let origin =
            TransferOrigin::from_url(&reqwest::Url::parse("https://example.com").unwrap()).unwrap();
        for addresses in [
            vec![
                "1.1.1.1:443".parse().unwrap(),
                "127.0.0.1:443".parse().unwrap(),
            ],
            vec!["1.1.1.1:80".parse().unwrap()],
            Vec::new(),
        ] {
            assert!(pinned_config(origin.clone(), addresses).is_err());
        }
        let config = pinned_config(origin, vec!["1.1.1.1:443".parse().unwrap()]).unwrap();
        assert_eq!(config.origin_count(), 1);
        assert_eq!(config.pinned_origin_count(), 1);
        assert_eq!(config.connect_timeout(), CONNECT_TIMEOUT);
        assert_eq!(config.idle_read_timeout(), IDLE_READ_TIMEOUT);
        assert_eq!(config.request_timeout(), REQUEST_TIMEOUT);
    }

    #[test]
    fn dns_answers_are_deduplicated_and_bounded_in_resolver_order() {
        let expected = (1..=32)
            .map(|last| SocketAddr::from(([1, 1, 1, last], 443)))
            .collect::<Vec<_>>();
        let answers = (1..=64).flat_map(|last| [SocketAddr::from(([1, 1, 1, last], 443)); 2]);
        assert_eq!(bounded_unique_addresses(answers), expected);
    }

    #[test]
    fn managed_retry_classifier_retains_transient_statuses_only() {
        for failure in [
            TransferFailureKind::Network,
            TransferFailureKind::ProviderStatus(408),
            TransferFailureKind::ProviderStatus(429),
            TransferFailureKind::ProviderStatus(503),
        ] {
            assert!(transfer_retryable(&failure));
        }
        for failure in [
            TransferFailureKind::ProviderStatus(400),
            TransferFailureKind::ProviderStatus(401),
            TransferFailureKind::ProviderStatus(404),
            TransferFailureKind::ProviderStatus(425),
        ] {
            assert!(!transfer_retryable(&failure));
        }
        let _policy = managed_transfer_retry_policy();
    }
}
