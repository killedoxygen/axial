use super::errors::{ApiError, json_error};
use axum::http::StatusCode;
use sha2::{Digest, Sha256};
use std::io::Cursor;

pub(super) const CAPE_TEXTURE_MAX_DIMENSION: u32 = 512;
pub(super) const SKIN_WIDTH: u32 = 64;
pub(super) const SKIN_HEIGHT: u32 = 64;
pub(super) const LEGACY_SKIN_HEIGHT: u32 = 32;
pub(super) const PNG_SIGNATURE: &[u8; 8] = b"\x89PNG\r\n\x1a\n";
pub const SKIN_PNG_MAX_BYTES: usize = 256 * 1024;
const SKIN_PNG_DECODER_BUDGET_BYTES: usize = SKIN_PNG_MAX_BYTES * 2;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SkinPngValidationError {
    TooLarge,
    InvalidPng,
    InvalidDimensions,
}

pub(super) struct NormalizedSkinPng {
    pub(super) original_width: u32,
    pub(super) original_height: u32,
    pub(super) variant_suggestion: &'static str,
    pub(super) png_bytes: Vec<u8>,
}

pub(super) fn normalize_skin_png(bytes: &[u8]) -> Result<NormalizedSkinPng, ApiError> {
    // Minecraft accepts 64x32 legacy skins, but Axial stores normalized 64x64 PNGs.
    if !bytes.starts_with(PNG_SIGNATURE) {
        return Err(json_error(
            StatusCode::BAD_REQUEST,
            "skin upload must be a PNG",
        ));
    }

    let decoded = decode_skin_png(bytes)?;
    let original_height = decoded.height;
    let legacy_shaped = original_height == LEGACY_SKIN_HEIGHT
        || (original_height == SKIN_HEIGHT
            && (is_padded_legacy_skin_rgba(&decoded.rgba)
                || (legacy_head_overlay_is_fully_opaque(&decoded.rgba)
                    && has_legacy_copied_limb_regions(&decoded.rgba))));
    let mut normalized_rgba = if legacy_shaped {
        normalize_legacy_skin_rgba(&decoded.rgba)
    } else {
        decoded.rgba
    };
    let variant_suggestion = if legacy_shaped {
        "classic"
    } else {
        suggest_skin_variant(&normalized_rgba)
    };
    force_skin_base_alpha(&mut normalized_rgba, variant_suggestion);
    let png_bytes = encode_skin_png(&normalized_rgba)?;

    Ok(NormalizedSkinPng {
        original_width: decoded.width,
        original_height,
        variant_suggestion,
        png_bytes,
    })
}

pub(super) struct DecodedSkinPng {
    pub(super) width: u32,
    pub(super) height: u32,
    pub(super) rgba: Vec<u8>,
}

pub fn validate_skin_png(bytes: &[u8]) -> Result<(), SkinPngValidationError> {
    decode_skin_png_validated(bytes).map(|_| ())
}

#[cfg(test)]
pub(super) fn validate_skin_png_with_budget(
    bytes: &[u8],
    decoder_budget_bytes: usize,
) -> Result<(), SkinPngValidationError> {
    decode_skin_png_with_budget(bytes, decoder_budget_bytes).map(|_| ())
}

pub(super) fn decode_skin_png(bytes: &[u8]) -> Result<DecodedSkinPng, ApiError> {
    decode_skin_png_validated(bytes).map_err(|error| match error {
        SkinPngValidationError::TooLarge => json_error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "skin upload is too large",
        ),
        SkinPngValidationError::InvalidPng => {
            json_error(StatusCode::BAD_REQUEST, "skin upload must be a valid PNG")
        }
        SkinPngValidationError::InvalidDimensions => json_error(
            StatusCode::BAD_REQUEST,
            "skin image must be 64x64 or 64x32",
        ),
    })
}

fn decode_skin_png_validated(
    bytes: &[u8],
) -> Result<DecodedSkinPng, SkinPngValidationError> {
    decode_skin_png_with_budget(bytes, SKIN_PNG_DECODER_BUDGET_BYTES)
}

fn decode_skin_png_with_budget(
    bytes: &[u8],
    decoder_budget_bytes: usize,
) -> Result<DecodedSkinPng, SkinPngValidationError> {
    if bytes.len() > SKIN_PNG_MAX_BYTES {
        return Err(SkinPngValidationError::TooLarge);
    }
    if !png_ends_exactly_at_iend(bytes) {
        return Err(SkinPngValidationError::InvalidPng);
    }
    let mut decoder = png::Decoder::new_with_limits(
        Cursor::new(bytes),
        png::Limits {
            bytes: decoder_budget_bytes,
        },
    );
    decoder.set_ignore_text_chunk(true);
    decoder.set_ignore_iccp_chunk(true);
    decoder.set_transformations(
        png::Transformations::EXPAND | png::Transformations::ALPHA | png::Transformations::STRIP_16,
    );
    let mut reader = decoder
        .read_info()
        .map_err(|_| SkinPngValidationError::InvalidPng)?;
    let info = reader.info();
    if info.width != SKIN_WIDTH
        || !matches!(info.height, LEGACY_SKIN_HEIGHT | SKIN_HEIGHT)
    {
        return Err(SkinPngValidationError::InvalidDimensions);
    }
    if info.animation_control.is_some() {
        return Err(SkinPngValidationError::InvalidPng);
    }

    let mut buffer = vec![0; reader.output_buffer_size()];
    let frame = reader
        .next_frame(&mut buffer)
        .map_err(|_| SkinPngValidationError::InvalidPng)?;
    if frame.width != SKIN_WIDTH
        || !matches!(frame.height, LEGACY_SKIN_HEIGHT | SKIN_HEIGHT)
    {
        return Err(SkinPngValidationError::InvalidDimensions);
    }
    let rgba = png_frame_to_rgba(
        &buffer[..frame.buffer_size()],
        frame.color_type,
        frame.bit_depth,
    )?;
    reader
        .finish()
        .map_err(|_| SkinPngValidationError::InvalidPng)?;

    Ok(DecodedSkinPng {
        width: frame.width,
        height: frame.height,
        rgba,
    })
}

fn png_ends_exactly_at_iend(bytes: &[u8]) -> bool {
    if !bytes.starts_with(PNG_SIGNATURE) {
        return false;
    }

    let mut offset = PNG_SIGNATURE.len();
    loop {
        let Some(header_end) = offset.checked_add(8) else {
            return false;
        };
        let Some(header) = bytes.get(offset..header_end) else {
            return false;
        };
        let length = u32::from_be_bytes([header[0], header[1], header[2], header[3]]) as usize;
        let Some(chunk_end) = header_end
            .checked_add(length)
            .and_then(|data_end| data_end.checked_add(4))
        else {
            return false;
        };
        if chunk_end > bytes.len() {
            return false;
        }
        if &header[4..8] == b"IEND" {
            return length == 0 && chunk_end == bytes.len();
        }
        offset = chunk_end;
    }
}

fn png_frame_to_rgba(
    data: &[u8],
    color_type: png::ColorType,
    bit_depth: png::BitDepth,
) -> Result<Vec<u8>, SkinPngValidationError> {
    if bit_depth != png::BitDepth::Eight {
        return Err(SkinPngValidationError::InvalidPng);
    }

    match color_type {
        png::ColorType::Rgba => Ok(data.to_vec()),
        png::ColorType::Rgb => {
            let mut rgba = Vec::with_capacity(data.len() / 3 * 4);
            for pixel in data.chunks_exact(3) {
                rgba.extend_from_slice(&[pixel[0], pixel[1], pixel[2], 255]);
            }
            Ok(rgba)
        }
        png::ColorType::GrayscaleAlpha => {
            let mut rgba = Vec::with_capacity(data.len() / 2 * 4);
            for pixel in data.chunks_exact(2) {
                rgba.extend_from_slice(&[pixel[0], pixel[0], pixel[0], pixel[1]]);
            }
            Ok(rgba)
        }
        png::ColorType::Grayscale => {
            let mut rgba = Vec::with_capacity(data.len() * 4);
            for value in data {
                rgba.extend_from_slice(&[*value, *value, *value, 255]);
            }
            Ok(rgba)
        }
        png::ColorType::Indexed => Err(SkinPngValidationError::InvalidPng),
    }
}

pub(super) fn normalize_legacy_skin_rgba(rgba: &[u8]) -> Vec<u8> {
    let mut normalized = vec![0; (SKIN_WIDTH * SKIN_HEIGHT * 4) as usize];
    let row_len = (SKIN_WIDTH * 4) as usize;
    for row in 0..LEGACY_SKIN_HEIGHT as usize {
        let offset = row * row_len;
        normalized[offset..offset + row_len].copy_from_slice(&rgba[offset..offset + row_len]);
    }
    if legacy_head_overlay_is_fully_opaque(&normalized) {
        clear_skin_region(&mut normalized, 32, 0, 32, 16);
    }
    copy_legacy_limb_region(&mut normalized, 4, 16, 4, 4, 20, 48);
    copy_legacy_limb_region(&mut normalized, 8, 16, 4, 4, 24, 48);
    copy_legacy_limb_region(&mut normalized, 0, 20, 4, 12, 24, 52);
    copy_legacy_limb_region(&mut normalized, 4, 20, 4, 12, 20, 52);
    copy_legacy_limb_region(&mut normalized, 8, 20, 4, 12, 16, 52);
    copy_legacy_limb_region(&mut normalized, 12, 20, 4, 12, 28, 52);

    copy_legacy_limb_region(&mut normalized, 44, 16, 4, 4, 36, 48);
    copy_legacy_limb_region(&mut normalized, 48, 16, 4, 4, 40, 48);
    copy_legacy_limb_region(&mut normalized, 40, 20, 4, 12, 40, 52);
    copy_legacy_limb_region(&mut normalized, 44, 20, 4, 12, 36, 52);
    copy_legacy_limb_region(&mut normalized, 48, 20, 4, 12, 32, 52);
    copy_legacy_limb_region(&mut normalized, 52, 20, 4, 12, 44, 52);

    normalized
}

fn copy_legacy_limb_region(
    rgba: &mut [u8],
    source_x: u32,
    source_y: u32,
    width: u32,
    height: u32,
    target_x: u32,
    target_y: u32,
) {
    for y in 0..height {
        for x in 0..width {
            let source_index = (((source_y + y) * SKIN_WIDTH + source_x + x) * 4) as usize;
            let target_index = (((target_y + y) * SKIN_WIDTH + target_x + x) * 4) as usize;
            let pixel = [
                rgba[source_index],
                rgba[source_index + 1],
                rgba[source_index + 2],
                rgba[source_index + 3],
            ];
            rgba[target_index..target_index + 4].copy_from_slice(&pixel);
        }
    }
}

fn is_padded_legacy_skin_rgba(rgba: &[u8]) -> bool {
    if rgba.len() < (SKIN_WIDTH * SKIN_HEIGHT * 4) as usize {
        return false;
    }

    for y in LEGACY_SKIN_HEIGHT..SKIN_HEIGHT {
        for x in 0..SKIN_WIDTH {
            let alpha_index = ((y * SKIN_WIDTH + x) * 4 + 3) as usize;
            if rgba[alpha_index] != 0 {
                return false;
            }
        }
    }
    true
}

fn legacy_head_overlay_is_fully_opaque(rgba: &[u8]) -> bool {
    if rgba.len() < (SKIN_WIDTH * SKIN_HEIGHT * 4) as usize {
        return false;
    }

    for y in 0..16 {
        for x in 32..64 {
            let alpha_index = ((y * SKIN_WIDTH + x) * 4 + 3) as usize;
            if rgba[alpha_index] != 255 {
                return false;
            }
        }
    }
    true
}

fn has_legacy_copied_limb_regions(rgba: &[u8]) -> bool {
    skin_regions_match(rgba, 0, 20, 24, 52, 4, 12)
        && skin_regions_match(rgba, 4, 20, 20, 52, 4, 12)
        && skin_regions_match(rgba, 40, 20, 40, 52, 4, 12)
        && skin_regions_match(rgba, 44, 20, 36, 52, 4, 12)
}

fn skin_regions_match(
    rgba: &[u8],
    source_x: u32,
    source_y: u32,
    target_x: u32,
    target_y: u32,
    width: u32,
    height: u32,
) -> bool {
    if rgba.len() < (SKIN_WIDTH * SKIN_HEIGHT * 4) as usize {
        return false;
    }

    for y in 0..height {
        for x in 0..width {
            let source_index = (((source_y + y) * SKIN_WIDTH + source_x + x) * 4) as usize;
            let target_index = (((target_y + y) * SKIN_WIDTH + target_x + x) * 4) as usize;
            if rgba[source_index..source_index + 4] != rgba[target_index..target_index + 4] {
                return false;
            }
        }
    }
    true
}

fn clear_skin_region(rgba: &mut [u8], start_x: u32, start_y: u32, width: u32, height: u32) {
    for y in start_y..start_y + height {
        for x in start_x..start_x + width {
            let index = ((y * SKIN_WIDTH + x) * 4) as usize;
            if let Some(pixel) = rgba.get_mut(index..index + 4) {
                pixel.copy_from_slice(&[0, 0, 0, 0]);
            }
        }
    }
}

fn force_skin_base_alpha(rgba: &mut [u8], variant: &str) {
    set_skin_base_region_alpha(rgba, 0, 0, 32, 16, variant);
    set_skin_base_region_alpha(rgba, 0, 16, 64, 16, variant);
    set_skin_base_region_alpha(rgba, 16, 48, 32, 16, variant);
}

fn has_transparent_skin_base_pixels(rgba: &[u8], variant: &str) -> bool {
    skin_base_region_has_transparent_pixels(rgba, 0, 0, 32, 16, variant)
        || skin_base_region_has_transparent_pixels(rgba, 0, 16, 64, 16, variant)
        || skin_base_region_has_transparent_pixels(rgba, 16, 48, 32, 16, variant)
}

fn set_skin_base_region_alpha(
    rgba: &mut [u8],
    start_x: u32,
    start_y: u32,
    width: u32,
    height: u32,
    variant: &str,
) {
    for y in start_y..start_y + height {
        for x in start_x..start_x + width {
            if is_slim_unused_arm_pixel(x, y, variant) {
                continue;
            }
            let alpha_index = ((y * SKIN_WIDTH + x) * 4 + 3) as usize;
            if let Some(alpha) = rgba.get_mut(alpha_index) {
                *alpha = 255;
            }
        }
    }
}

fn skin_base_region_has_transparent_pixels(
    rgba: &[u8],
    start_x: u32,
    start_y: u32,
    width: u32,
    height: u32,
    variant: &str,
) -> bool {
    for y in start_y..start_y + height {
        for x in start_x..start_x + width {
            if is_slim_unused_arm_pixel(x, y, variant) {
                continue;
            }
            let alpha_index = ((y * SKIN_WIDTH + x) * 4 + 3) as usize;
            if rgba.get(alpha_index).copied().unwrap_or(255) != 255 {
                return true;
            }
        }
    }
    false
}

fn is_slim_unused_arm_pixel(x: u32, y: u32, variant: &str) -> bool {
    variant == "slim"
        && (((54..56).contains(&x) && (20..32).contains(&y))
            || ((46..48).contains(&x) && (52..64).contains(&y)))
}

fn suggest_skin_variant(rgba: &[u8]) -> &'static str {
    // Slim skins leave the classic right-arm strip transparent in the normalized texture.
    for y in 20..32 {
        for x in 54..56 {
            let alpha_index = ((y * SKIN_WIDTH + x) * 4 + 3) as usize;
            if rgba.get(alpha_index).copied().unwrap_or(255) != 0 {
                return "classic";
            }
        }
    }
    "slim"
}

pub(super) fn render_skin_head_png(skin_png: &[u8], size: u32) -> Result<Vec<u8>, ApiError> {
    let decoded = decode_skin_png(skin_png)?;
    let mut head_rgba = vec![0; (size * size * 4) as usize];
    draw_scaled_skin_region(&decoded.rgba, &mut head_rgba, 8, 8, size, false);
    draw_scaled_skin_region(&decoded.rgba, &mut head_rgba, 40, 8, size, true);
    encode_rgba_png(&head_rgba, size, size, "failed to build skin head image")
}

fn draw_scaled_skin_region(
    source_rgba: &[u8],
    target_rgba: &mut [u8],
    source_x: u32,
    source_y: u32,
    target_size: u32,
    blend: bool,
) {
    for target_y in 0..target_size {
        for target_x in 0..target_size {
            let skin_x = source_x + target_x * 8 / target_size;
            let skin_y = source_y + target_y * 8 / target_size;
            let source_index = ((skin_y * SKIN_WIDTH + skin_x) * 4) as usize;
            let target_index = ((target_y * target_size + target_x) * 4) as usize;
            let source_pixel = [
                source_rgba[source_index],
                source_rgba[source_index + 1],
                source_rgba[source_index + 2],
                source_rgba[source_index + 3],
            ];

            if blend {
                blend_rgba_pixel(target_rgba, target_index, source_pixel);
            } else {
                target_rgba[target_index..target_index + 4].copy_from_slice(&source_pixel);
            }
        }
    }
}

fn blend_rgba_pixel(target_rgba: &mut [u8], target_index: usize, source: [u8; 4]) {
    let source_alpha = source[3] as u16;
    if source_alpha == 0 {
        return;
    }
    if source_alpha == 255 {
        target_rgba[target_index..target_index + 4].copy_from_slice(&source);
        return;
    }

    let inverse_alpha = 255 - source_alpha;
    for channel in 0..3 {
        let source_channel = source[channel] as u16;
        let target_channel = target_rgba[target_index + channel] as u16;
        target_rgba[target_index + channel] =
            ((source_channel * source_alpha + target_channel * inverse_alpha) / 255) as u8;
    }
    let target_alpha = target_rgba[target_index + 3] as u16;
    target_rgba[target_index + 3] =
        (source_alpha + target_alpha * inverse_alpha / 255).min(255) as u8;
}

fn encode_skin_png(rgba: &[u8]) -> Result<Vec<u8>, ApiError> {
    encode_rgba_png(
        rgba,
        SKIN_WIDTH,
        SKIN_HEIGHT,
        "failed to normalize skin image",
    )
}

fn encode_rgba_png(
    rgba: &[u8],
    width: u32,
    height: u32,
    error_message: &'static str,
) -> Result<Vec<u8>, ApiError> {
    let mut bytes = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut bytes, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder
            .write_header()
            .map_err(|_| json_error(StatusCode::INTERNAL_SERVER_ERROR, error_message))?;
        writer
            .write_image_data(rgba)
            .map_err(|_| json_error(StatusCode::INTERNAL_SERVER_ERROR, error_message))?;
    }
    Ok(bytes)
}

pub(super) fn texture_key(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut key = String::with_capacity(digest.len() * 2);
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for byte in digest {
        key.push(HEX[(byte >> 4) as usize] as char);
        key.push(HEX[(byte & 0x0f) as usize] as char);
    }
    key
}

pub(super) fn is_valid_normalized_skin_cache_png(bytes: &[u8]) -> bool {
    if !bytes.starts_with(PNG_SIGNATURE) {
        return false;
    }

    decode_skin_png(bytes).is_ok_and(|decoded| {
        decoded.width == SKIN_WIDTH
            && decoded.height == SKIN_HEIGHT
            && !has_transparent_skin_base_pixels(&decoded.rgba, suggest_skin_variant(&decoded.rgba))
            && !(legacy_head_overlay_is_fully_opaque(&decoded.rgba)
                && has_legacy_copied_limb_regions(&decoded.rgba))
    })
}

pub(super) fn is_valid_cape_texture_png(bytes: &[u8]) -> bool {
    if !bytes.starts_with(PNG_SIGNATURE) {
        return false;
    }
    let decoder = png::Decoder::new(Cursor::new(bytes));
    let Ok(reader) = decoder.read_info() else {
        return false;
    };
    let info = reader.info();
    info.width > 0
        && info.height > 0
        && info.width <= CAPE_TEXTURE_MAX_DIMENSION
        && info.height <= CAPE_TEXTURE_MAX_DIMENSION
}
