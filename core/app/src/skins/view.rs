//! Public acknowledgements for profile skin commands.

use super::pending::PendingSkinStatus;
use crate::public::CommandSummary;
use serde::Serialize;
use ts_rs::TS;

#[derive(Clone, Debug, Serialize, TS)]
pub struct SkinApplyResponse {
    pub pending: Option<PendingSkinStatus>,
    pub profile_updated: bool,
    pub status: &'static str,
    pub texture_key: String,
    pub view_model: CommandSummary,
}

impl SkinApplyResponse {
    /// A deferred choice stays pending until a flush applies it.
    pub fn queued(texture_key: String, pending: PendingSkinStatus) -> Self {
        Self {
            pending: Some(pending),
            profile_updated: false,
            status: "queued",
            texture_key,
            view_model: CommandSummary {
                summary: "Skin queued.",
            },
        }
    }

    pub fn applied(texture_key: String) -> Self {
        Self {
            pending: None,
            profile_updated: true,
            status: "applied",
            texture_key,
            view_model: CommandSummary {
                summary: "Skin applied.",
            },
        }
    }
}

#[derive(Clone, Debug, Serialize, TS)]
pub struct SkinFlushResponse {
    pub applied: usize,
    pub status: &'static str,
    pub view_model: CommandSummary,
}

impl SkinFlushResponse {
    pub fn new(applied: usize) -> Self {
        Self {
            applied,
            status: "flushed",
            view_model: CommandSummary {
                summary: if applied > 0 {
                    "Skin applied."
                } else {
                    "No skin change queued."
                },
            },
        }
    }
}

#[derive(Clone, Debug, Serialize, TS)]
pub struct SkinCancelResponse {
    pub cleared: bool,
    pub status: &'static str,
    pub view_model: CommandSummary,
}

impl SkinCancelResponse {
    pub fn new(cleared: bool) -> Self {
        Self {
            cleared,
            status: "cleared",
            view_model: CommandSummary {
                summary: "Skin change canceled.",
            },
        }
    }
}

#[derive(Clone, Debug, Serialize, TS)]
pub struct ProfileResetResponse {
    pub profile_updated: bool,
    pub status: &'static str,
    pub view_model: CommandSummary,
}

impl ProfileResetResponse {
    /// `skin` resets the profile skin; otherwise its cape.
    pub fn new(skin: bool) -> Self {
        Self {
            profile_updated: true,
            status: "reset",
            view_model: CommandSummary {
                summary: if skin {
                    "Profile skin reset."
                } else {
                    "Cape reset."
                },
            },
        }
    }
}
