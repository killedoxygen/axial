//! Declarative composition selection from the active manifest.

mod app_version;
mod error;
mod hardware;
mod planner;
mod validation;
mod version;

#[cfg(test)]
mod tests;

pub use error::ResolveError;
pub use hardware::detect_hardware;
pub use planner::{classify_version, parse_mode, resolve_plan};
pub use validation::{PERFORMANCE_MANIFEST_SCHEMA_VERSION, builtin_manifest, validate_manifest};
