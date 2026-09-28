//! User-owned instance resources. File selectors never confer root authority.

pub mod folders;
pub mod mods;
pub mod screenshots;
mod service;
pub mod worlds;
pub use service::{
    InstanceLogInfo, InstanceLogTailResponse, InstanceResourcesResponse, ResourceCommand,
    ResourceError, ResourceService,
};

fn timestamp(nanoseconds: u64) -> String {
    i64::try_from(nanoseconds / 1_000_000_000)
        .ok()
        .and_then(|seconds| {
            chrono::DateTime::<chrono::Utc>::from_timestamp(
                seconds,
                (nanoseconds % 1_000_000_000) as u32,
            )
        })
        .map(|time| time.to_rfc3339())
        .unwrap_or_default()
}
