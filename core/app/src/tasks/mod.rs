//! Retained asynchronous work, admission leases and latest projections.
//!
//! These primitives do not decide domain outcomes or persist recovery records.
//! An accepted future must settle its effects, or transfer its retained guards
//! into a domain-owned receipt, before returning. Dropping a waiter never drops
//! that future. Shutdown requests cancellation; it never aborts unsettled work.

mod cancellation;
mod exclusion;
mod owner;
mod projection;

pub use cancellation::CancellationToken;
pub use exclusion::{ArtifactKey, ExclusionError, ExclusionLease, Exclusions};
pub use owner::{
    OwnerBusy, OwnerSnapshot, ShutdownError, ShutdownReceipt, SpawnError, TaskHandle, TaskId,
    TaskJoinError, TaskOwner,
};
pub use projection::{Projection, ProjectionError, ProjectionSubscription, Revisioned};
