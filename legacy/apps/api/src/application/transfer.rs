//! Application-owned public network admission for managed transfers.

use axial_minecraft::download::{
    PinnedTransferOrigin, RetryPolicy, TransferClient, TransferClientConfig, TransferFailureKind,
    TransferOrigin,
};
use std::collections::HashSet;
use std::io;
use std::net::SocketAddr;
use std::time::Duration;

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

pub(crate) async fn pinned_public_transfer_client(
    origin: TransferOrigin,
    url: &reqwest::Url,
) -> io::Result<TransferClient> {
    let host = url
        .host_str()
        .ok_or_else(transfer_admission_error)?
        .to_string();
    let port = url
        .port_or_known_default()
        .ok_or_else(transfer_admission_error)?;
    let resolved =
        tokio::time::timeout(DNS_TIMEOUT, tokio::net::lookup_host((host.as_str(), port)))
            .await
            .map_err(|_| transfer_admission_error())?
            .map_err(|_| transfer_admission_error())?;
    let pinned = PinnedTransferOrigin::public(origin, bounded_unique_addresses(resolved))
        .map_err(|_| transfer_admission_error())?;
    let config = TransferClientConfig::bounded_pinned_public(
        CONNECT_TIMEOUT,
        IDLE_READ_TIMEOUT,
        REQUEST_TIMEOUT,
        vec![pinned],
    )
    .map_err(|_| transfer_admission_error())?;
    TransferClient::build(config).map_err(|_| transfer_admission_error())
}

pub(crate) fn managed_transfer_retry_policy() -> RetryPolicy {
    RetryPolicy::classified(&RETRY_DELAYS, transfer_retryable)
        .expect("fixed managed transfer retry policy is valid")
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

fn transfer_admission_error() -> io::Error {
    io::Error::other("public transfer origin could not be pinned")
}
