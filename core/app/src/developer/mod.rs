//! Local feature flags and development-only diagnostics.
//!
//! This module does not own launch state, settings persistence, or root deletion.
//! Those owners supply committed snapshots and retained filesystem authority.

#[cfg(debug_assertions)]
pub mod command;
