//! Shared provider transport. Only complete, bounded bodies escape this module.
//!
//! A matching provider checksum establishes agreement with the supplied metadata,
//! not publisher authenticity. Downloading never grants filesystem publication
//! authority; consumers retain their managed-file and operation capabilities.

mod client;
mod managed;
mod policy;

pub use client::{DownloadEvidence, DownloadedBytes, ProviderClient};
pub use managed::{managed_transfer_retry_policy, pinned_public_transfer_client};
pub use policy::{
    Checksum, ClientConfig, DownloadError, DownloadRequest, HashAlgorithm, IntegrityEvidence,
    IntegrityPolicy, OriginPolicy, ResponseLimits, UnhashedReason,
};

#[cfg(test)]
mod tests;
