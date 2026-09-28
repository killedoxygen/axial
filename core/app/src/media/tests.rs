use super::*;
use std::io::Cursor;

pub(super) fn png(width: u32, height: u32, rgba: &[u8]) -> Vec<u8> {
    let mut bytes = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut bytes, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        encoder
            .write_header()
            .unwrap()
            .write_image_data(rgba)
            .unwrap();
    }
    bytes
}

fn rgba(bytes: &[u8]) -> Vec<u8> {
    let mut reader = png::Decoder::new(Cursor::new(bytes)).read_info().unwrap();
    let mut rgba = vec![0; reader.output_buffer_size()];
    let frame = reader.next_frame(&mut rgba).unwrap();
    rgba.truncate(frame.buffer_size());
    rgba
}

fn pixel(bytes: &[u8], x: usize, y: usize) -> &[u8] {
    &bytes[(y * 64 + x) * 4..(y * 64 + x + 1) * 4]
}

#[test]
fn legacy_conversion_preserves_faces_copies_limbs_and_removes_opaque_hat() {
    let mut source = vec![0; 64 * 32 * 4];
    for y in 0..32 {
        for x in 0..64 {
            source[(y * 64 + x) * 4..(y * 64 + x + 1) * 4]
                .copy_from_slice(&[x as u8, y as u8, 80, 255]);
        }
    }
    let normalized = normalize_skin_png(&png(64, 32, &source)).unwrap();
    assert_eq!(
        (normalized.original_width, normalized.original_height),
        (64, 32)
    );
    assert_eq!(normalized.variant_suggestion, SkinVariant::Classic);
    let decoded = rgba(&normalized.png_bytes);
    assert_eq!(decoded.len(), 64 * 64 * 4);
    assert_eq!(pixel(&decoded, 8, 8), &[8, 8, 80, 255]);
    assert_eq!(pixel(&decoded, 40, 8), &[0, 0, 0, 0]);
    assert_eq!(pixel(&decoded, 24, 52), &[0, 20, 80, 255]);
    assert_eq!(pixel(&decoded, 36, 52), &[44, 20, 80, 255]);
    assert_eq!(pixel(&decoded, 0, 40), &[0, 0, 0, 0]);
    assert_eq!(
        normalize_skin_png(&normalized.png_bytes).unwrap().png_bytes,
        normalized.png_bytes
    );
}

#[test]
fn padded_legacy_and_legacy_input_share_texture_identity() {
    let source = vec![45; 64 * 32 * 4];
    let mut padded = source.clone();
    padded.resize(64 * 64 * 4, 0);
    let legacy = normalize_skin_png(&png(64, 32, &source)).unwrap();
    let modern = normalize_skin_png(&png(64, 64, &padded)).unwrap();
    assert_eq!(modern.png_bytes, legacy.png_bytes);
    assert_eq!(
        texture_key(&modern.png_bytes),
        texture_key(&legacy.png_bytes)
    );
}

#[test]
fn slim_strip_remains_transparent_while_base_alpha_is_fixed() {
    let mut source = vec![50; 64 * 64 * 4];
    for (start_x, start_y) in [(54, 20), (46, 52)] {
        for y in start_y..start_y + 12 {
            for x in start_x..start_x + 2 {
                source[(y * 64 + x) * 4 + 3] = 0;
            }
        }
    }
    let normalized = normalize_skin_png(&png(64, 64, &source)).unwrap();
    assert_eq!(normalized.variant_suggestion, SkinVariant::Slim);
    let decoded = rgba(&normalized.png_bytes);
    assert_eq!(pixel(&decoded, 8, 8)[3], 255);
    assert_eq!(pixel(&decoded, 54, 20)[3], 0);
    assert_eq!(pixel(&decoded, 46, 52)[3], 0);
    assert_eq!(pixel(&decoded, 40, 8)[3], 50);
    assert!(is_valid_normalized_skin_cache_png(&normalized.png_bytes));
}

#[test]
fn decode_rejects_bad_dimensions_trailing_data_truncation_crc_and_budget() {
    assert_eq!(
        validate_skin_png(&png(65, 64, &vec![0; 65 * 64 * 4])),
        Err(MediaError::InvalidDimensions)
    );
    let source = png(64, 64, &vec![23; 64 * 64 * 4]);
    let mut trailing = source.clone();
    trailing.push(0);
    assert_eq!(validate_skin_png(&trailing), Err(MediaError::InvalidPng));
    assert_eq!(
        validate_skin_png(&source[..source.len() - 1]),
        Err(MediaError::InvalidPng)
    );
    let mut corrupt = source.clone();
    corrupt[29] ^= 1;
    assert_eq!(validate_skin_png(&corrupt), Err(MediaError::InvalidPng));
    assert!(image::validate_skin_png_with_budget(&source, 1).is_err());
    let mut oversized = source;
    oversized.resize(SKIN_PNG_MAX_BYTES + 1, 0);
    assert_eq!(validate_skin_png(&oversized), Err(MediaError::TooLarge));
}

#[test]
fn animation_is_rejected_for_skins_and_capes() {
    let mut bytes = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut bytes, 64, 64);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        encoder.set_animated(1, 0).unwrap();
        encoder
            .write_header()
            .unwrap()
            .write_image_data(&vec![100; 64 * 64 * 4])
            .unwrap();
    }
    assert_eq!(validate_skin_png(&bytes), Err(MediaError::InvalidPng));
    assert_eq!(normalize_cape_png(&bytes), Err(MediaError::InvalidPng));
}

#[test]
fn capes_require_a_complete_bounded_frame_not_just_valid_header() {
    let bytes = png(64, 32, &vec![22; 64 * 32 * 4]);
    assert!(is_valid_cape_texture_png(&bytes));
    assert_eq!(
        rgba(&normalize_cape_png(&bytes).unwrap()),
        vec![22; 64 * 32 * 4]
    );
    assert!(!is_valid_cape_texture_png(&bytes[..33]));
    let huge = png(513, 1, &vec![22; 513 * 4]);
    assert_eq!(
        normalize_cape_png(&huge),
        Err(MediaError::InvalidDimensions)
    );
    assert_eq!(
        normalize_cape_png(&vec![0; CAPE_PNG_MAX_BYTES + 1]),
        Err(MediaError::TooLarge)
    );
}

#[test]
fn grayscale_palette_and_sixteen_bit_inputs_normalize_to_rgba() {
    for (color, depth, samples, expected) in [
        (
            png::ColorType::Grayscale,
            png::BitDepth::Eight,
            vec![71; 64 * 64],
            [71, 71, 71, 255],
        ),
        (
            png::ColorType::GrayscaleAlpha,
            png::BitDepth::Eight,
            [81, 7].repeat(64 * 64),
            [81, 81, 81, 255],
        ),
        (
            png::ColorType::Rgb,
            png::BitDepth::Sixteen,
            [90, 1, 80, 2, 70, 3].repeat(64 * 64),
            [90, 80, 70, 255],
        ),
        (
            png::ColorType::Indexed,
            png::BitDepth::Eight,
            vec![0; 64 * 64],
            [63, 72, 81, 255],
        ),
    ] {
        let mut bytes = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut bytes, 64, 64);
            encoder.set_color(color);
            encoder.set_depth(depth);
            if color == png::ColorType::Indexed {
                encoder.set_palette(vec![63, 72, 81]);
                encoder.set_trns(vec![33]);
            }
            encoder
                .write_header()
                .unwrap()
                .write_image_data(&samples)
                .unwrap();
        }
        validate_skin_png(&bytes).unwrap();
        let normalized = normalize_skin_png(&bytes).unwrap();
        assert_eq!(
            pixel(&rgba(&normalized.png_bytes), 8, 8),
            expected,
            "{color:?} {depth:?}"
        );
    }
}

#[test]
fn head_overlay_blends_and_rendering_is_bounded() {
    let mut source = vec![0; 64 * 64 * 4];
    for y in 8..16 {
        for x in 8..16 {
            source[(y * 64 + x) * 4..(y * 64 + x + 1) * 4].copy_from_slice(&[200, 100, 0, 255]);
            source[(y * 64 + x + 32) * 4..(y * 64 + x + 33) * 4]
                .copy_from_slice(&[0, 100, 200, 128]);
        }
    }
    let source = png(64, 64, &source);
    let head = rgba(&render_skin_head_png(&source, 1).unwrap());
    assert_eq!(head, [99, 100, 100, 255]);
    assert_eq!(
        render_skin_head_png(&source, 0),
        Err(MediaError::InvalidDimensions)
    );
    assert_eq!(
        render_skin_head_png(&source, 513),
        Err(MediaError::InvalidDimensions)
    );
    assert_eq!(
        texture_key(b"abc"),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
}
