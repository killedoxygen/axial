mod cache;
mod normalize;
mod query;

pub use normalize::normalize_supported_versions;

pub use query::{
    fetch_builds, fetch_builds_cancellable, fetch_cached_builds, fetch_components,
    fetch_supported_versions, fetch_supported_versions_cancellable,
    resolve_build_record_for_install,
};
#[cfg(feature = "test-support")]
pub use query::{
    fetch_fabric_builds_for_test, fetch_fabric_game_versions_for_test,
    persist_loader_build_cache_fixture_for_test,
    persist_loader_supported_versions_cache_fixture_for_test, resolve_fabric_build_for_test,
};
