//! Retained asynchronous work and admission leases.
//!
//! These primitives do not decide domain outcomes or persist recovery records.
//! An accepted future must settle its effects, or transfer its retained guards
//! into a domain-owned receipt, before returning. Dropping a waiter never drops
//! that future. Shutdown requests cancellation; it never aborts unsettled work.

mod cancellation;
mod exclusion;
mod owner;

pub use cancellation::CancellationToken;
pub use exclusion::{ArtifactKey, ExclusionError, ExclusionLease, Exclusions};
pub use owner::{
    OwnerBusy, OwnerSnapshot, ShutdownError, ShutdownReceipt, SpawnError, TaskHandle, TaskId,
    TaskJoinError, TaskOwner,
};

const SETTLE_RETRY: std::time::Duration = std::time::Duration::from_secs(2);

/// Retry one owned settlement until it succeeds. A failed attempt leaves the
/// caller's owners retained; nothing is released as if it had settled.
pub async fn retry_until_settled<T, E, F>(what: &'static str, mut attempt: impl FnMut() -> F) -> T
where
    E: std::fmt::Display,
    F: std::future::Future<Output = Result<T, E>>,
{
    loop {
        match attempt().await {
            Ok(value) => return value,
            Err(error) => tracing::warn!(%error, "{what} remains unsettled; retrying"),
        }
        tokio::time::sleep(SETTLE_RETRY).await;
    }
}
