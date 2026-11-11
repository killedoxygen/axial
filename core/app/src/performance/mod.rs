//! User-selected Performance behavior and exact managed-file transactions.

pub mod benchmarks;
pub mod composition;
pub(crate) mod duplicate;
pub mod effective;
pub mod health;
pub mod model;
pub mod mutation;
pub mod plan;
pub mod proofs;
pub mod qualification;
pub mod resolve;
pub mod rollback;
pub mod rules;
#[cfg(test)]
mod tests;

pub use mutation::{PerformanceMutationError, PerformanceService, PreparedPerformance};

pub fn public_transfer_resolver() -> composition::ManagedArtifactTransferResolver {
    composition::ManagedArtifactTransferResolver::new(
        |url| async move {
            let origin = axial_minecraft::download::TransferOrigin::from_url(&url)
                .map_err(|_| std::io::Error::other("managed artifact origin was not admitted"))?;
            crate::network::pinned_public_transfer_client(origin, &url).await
        },
        crate::network::managed_transfer_retry_policy(),
    )
}
