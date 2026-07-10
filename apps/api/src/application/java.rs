//! Application-owned Java runtime query boundary.
//!
//! Core Minecraft code owns the primitive runtime discovery. Application owns
//! the route-facing query workflow and keeps route handlers as transport
//! adapters.

use crate::state::AppState;
use axial_minecraft::{JavaRuntimeResult, list_java_runtimes};
use serde::Serialize;
use std::path::PathBuf;

#[derive(Debug, Serialize)]
pub struct JavaRuntimesResponse {
    pub runtimes: Vec<JavaRuntimeResult>,
}

pub fn java_runtimes(state: &AppState) -> JavaRuntimesResponse {
    JavaRuntimesResponse {
        runtimes: java_runtimes_for_library_dir(state.library_dir()),
    }
}

fn java_runtimes_for_library_dir(library_dir: Option<String>) -> Vec<JavaRuntimeResult> {
    library_dir
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
        .map(|path| list_java_runtimes(&path))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::java_runtimes_for_library_dir;

    #[test]
    fn ignores_undefined_or_blank_library_dir() {
        assert!(java_runtimes_for_library_dir(None).is_empty());
        assert!(java_runtimes_for_library_dir(Some(String::new())).is_empty());
    }
}
