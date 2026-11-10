//! Bounded image normalization and scoped native selection authority.

mod image;
mod native;

pub use image::{
    CAPE_PNG_MAX_BYTES, NormalizedSkinPng, SKIN_PNG_MAX_BYTES, SkinPngValidationError,
    is_valid_cape_texture_png, is_valid_normalized_skin_cache_png, normalize_cape_png,
    normalize_skin_png, texture_key, validate_skin_png,
};
pub use native::{
    AdmittedSkinFile, NativeSelection, NativeSkinAdmission, NativeSkinHandle, NativeSkinScope,
};

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "lowercase")]
pub enum SkinVariant {
    Classic,
    Slim,
}

impl SkinVariant {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Classic => "classic",
            Self::Slim => "slim",
        }
    }

    /// Minecraft services name the model in varying case; anything but slim is classic.
    pub fn from_provider(value: &str) -> Self {
        if value.eq_ignore_ascii_case("slim") {
            Self::Slim
        } else {
            Self::Classic
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum MediaError {
    #[error("Image is too large.")]
    TooLarge,
    #[error("Choose a valid PNG image.")]
    InvalidPng,
    #[error("Image dimensions are not supported.")]
    InvalidDimensions,
    #[error("Could not encode the image.")]
    EncodingFailed,
    #[error("Choose the skin file again.")]
    InvalidSelection,
    #[error("Skin file selection is no longer available.")]
    Closed,
    #[error("Another skin selection is still being checked.")]
    Busy,
    #[error("Could not read skin file.")]
    ReadFailed,
    #[error("Skin file changed while it was being read. Choose it again.")]
    FileChanged,
}

#[cfg(test)]
mod tests;
