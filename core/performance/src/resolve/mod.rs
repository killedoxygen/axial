mod app_version;
mod hardware;
mod model;
mod planner;
mod validation;
mod version;

#[cfg(test)]
mod tests;

pub use hardware::detect_hardware;
pub use model::ResolveError;
pub use planner::{classify_version, parse_mode, resolve_plan};
pub use validation::{builtin_manifest, validate_manifest};
