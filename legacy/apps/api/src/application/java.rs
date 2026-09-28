//! Application-owned Java runtime query boundary.
//!
//! Core Minecraft code owns the primitive runtime discovery. Application owns
//! the route-facing query workflow and keeps route handlers as transport
//! adapters.

use crate::state::AppState;
use axial_minecraft::{JavaRuntimeResult, list_java_runtimes};
use serde::Serialize;

#[derive(Debug, Serialize)]
pub struct JavaRuntimesResponse {
    pub runtimes: Vec<JavaRuntimeResult>,
}

pub fn java_runtimes(state: &AppState) -> JavaRuntimesResponse {
    JavaRuntimesResponse {
        runtimes: list_java_runtimes(state.managed_runtime_cache()),
    }
}
