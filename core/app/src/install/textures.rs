//! Block textures of an installed version, read from its own client archive. The launcher draws its
//! picture of an instance with them, so each version looks like itself.

use super::{artifacts::VersionSummary, queue::InstallError};
use base64::Engine;
use serde::Serialize;
use std::io::{Cursor, Read};
use ts_rs::TS;

/// One side of a block texture, in texels.
const SIDE: u32 = 16;
const ARCHIVE_MAX_BYTES: u64 = 128 << 20;
const ENTRY_MAX_BYTES: u64 = 64 << 10;
const DECODER_BUDGET_BYTES: usize = 1 << 20;

/// Each texture by its current name, then the names earlier versions gave it, as paths under `textures/`.
const TEXTURES: &[(&str, &[&str])] = &[
    ("grass_block_top", &["blocks/grass_top"]),
    ("grass_block_side", &["blocks/grass_side"]),
    ("grass_block_side_overlay", &["blocks/grass_side_overlay"]),
    ("grass_block_snow", &["blocks/grass_side_snowed"]),
    ("dirt", &["blocks/dirt"]),
    ("podzol_top", &["blocks/dirt_podzol_top"]),
    ("podzol_side", &["blocks/dirt_podzol_side"]),
    ("stone", &["blocks/stone"]),
    ("andesite", &["blocks/stone_andesite"]),
    ("granite", &["blocks/stone_granite"]),
    ("coal_ore", &["blocks/coal_ore", "blocks/oreCoal"]),
    ("gravel", &["blocks/gravel"]),
    ("sand", &["blocks/sand"]),
    (
        "sandstone",
        &["blocks/sandstone_normal", "blocks/sandstone_side"],
    ),
    ("red_sand", &["blocks/red_sand"]),
    ("snow", &["blocks/snow"]),
    ("ice", &["blocks/ice"]),
    ("obsidian", &["blocks/obsidian"]),
    ("crying_obsidian", &[]),
    ("netherrack", &["blocks/netherrack", "blocks/hellrock"]),
    ("magma", &["blocks/magma"]),
    ("terracotta", &["blocks/hardened_clay"]),
    (
        "orange_terracotta",
        &["blocks/hardened_clay_stained_orange"],
    ),
    (
        "yellow_terracotta",
        &["blocks/hardened_clay_stained_yellow"],
    ),
    ("brown_terracotta", &["blocks/hardened_clay_stained_brown"]),
    ("red_terracotta", &["blocks/hardened_clay_stained_red"]),
    ("white_terracotta", &["blocks/hardened_clay_stained_white"]),
    (
        "light_gray_terracotta",
        &["blocks/hardened_clay_stained_silver"],
    ),
    ("lava_still", &["blocks/lava_still", "blocks/lava"]),
    ("oak_log", &["blocks/log_oak", "blocks/tree_side"]),
    ("oak_log_top", &["blocks/log_oak_top", "blocks/tree_top"]),
    ("birch_log", &["blocks/log_birch", "blocks/tree_birch"]),
    (
        "birch_log_top",
        &["blocks/log_birch_top", "blocks/tree_top"],
    ),
    ("spruce_log", &["blocks/log_spruce", "blocks/tree_spruce"]),
    (
        "spruce_log_top",
        &["blocks/log_spruce_top", "blocks/tree_top"],
    ),
    ("dark_oak_log", &["blocks/log_big_oak"]),
    ("dark_oak_log_top", &["blocks/log_big_oak_top"]),
    ("acacia_log", &["blocks/log_acacia"]),
    ("acacia_log_top", &["blocks/log_acacia_top"]),
    ("cherry_log", &[]),
    ("cherry_log_top", &[]),
    ("oak_leaves", &["blocks/leaves_oak", "blocks/leaves"]),
    ("birch_leaves", &["blocks/leaves_birch", "blocks/leaves"]),
    ("spruce_leaves", &["blocks/leaves_spruce"]),
    ("dark_oak_leaves", &["blocks/leaves_big_oak"]),
    ("acacia_leaves", &["blocks/leaves_acacia"]),
    ("cherry_leaves", &[]),
    ("vine", &["blocks/vine"]),
    ("cactus_side", &["blocks/cactus_side"]),
    ("cactus_top", &["blocks/cactus_top"]),
    ("short_grass", &["block/grass", "blocks/tallgrass"]),
    ("tall_grass_bottom", &["blocks/double_plant_grass_bottom"]),
    ("tall_grass_top", &["blocks/double_plant_grass_top"]),
    ("fern", &["blocks/fern"]),
    ("poppy", &["blocks/flower_rose", "blocks/rose"]),
    ("dandelion", &["blocks/flower_dandelion", "blocks/flower"]),
    ("cornflower", &[]),
    ("oxeye_daisy", &["blocks/flower_oxeye_daisy"]),
    ("azure_bluet", &["blocks/flower_houstonia"]),
    ("allium", &["blocks/flower_allium"]),
    ("blue_orchid", &["blocks/flower_blue_orchid"]),
    ("dead_bush", &["blocks/deadbush"]),
    ("sugar_cane", &["blocks/reeds"]),
    ("red_mushroom", &["blocks/mushroom_red"]),
    ("brown_mushroom", &["blocks/mushroom_brown"]),
    ("lily_pad", &["blocks/waterlily"]),
    ("pink_petals", &[]),
    ("firefly_bush", &[]),
    ("firefly_bush_emissive", &[]),
];

#[derive(Clone, Debug, Serialize, TS)]
pub struct BlockTexturesResponse {
    /// The textures this version has; one from a later version is simply absent.
    pub textures: Vec<BlockTexture>,
}

#[derive(Clone, Debug, Serialize, TS)]
pub struct BlockTexture {
    /// The texture's current name, whatever this version's archive calls it.
    pub name: String,
    /// Sixteen rows of sixteen RGBA texels, base64.
    pub rgba: String,
}

/// Reads from the version's own archive, or from the one it inherits when its own holds no textures.
pub(crate) fn read(
    summary: &VersionSummary,
    version_id: &str,
) -> Result<BlockTexturesResponse, InstallError> {
    let mut readable = false;
    for id in [version_id, summary.version.inherits_from.as_str()] {
        let Ok(file) = summary.client_archive(id) else {
            continue;
        };
        if file.size() > ARCHIVE_MAX_BYTES {
            continue;
        }
        let work = axial_resource::process_physical_work();
        let scratch = work
            .try_reserve_scratch(file.size())
            .map_err(|_| InstallError::AtCapacity)?;
        let textures = work
            .try_run_inline(
                axial_resource::PhysicalWorkRequest::foreground(
                    axial_resource::PhysicalIoClass::Read,
                    0,
                ),
                || file.read_bounded(ARCHIVE_MAX_BYTES).map(extract),
            )
            .map_err(|_| InstallError::AtCapacity)?;
        drop(scratch);
        let Ok(textures) = textures else { continue };
        readable = true;
        if !textures.is_empty() {
            return Ok(BlockTexturesResponse { textures });
        }
    }
    if readable {
        Ok(BlockTexturesResponse {
            textures: Vec::new(),
        })
    } else {
        Err(InstallError::NotReady)
    }
}

fn extract(archive: Vec<u8>) -> Vec<BlockTexture> {
    let Ok(mut archive) = zip::ZipArchive::new(Cursor::new(archive)) else {
        return Vec::new();
    };
    let mut textures = Vec::new();
    for (name, earlier) in TEXTURES {
        let current = format!("assets/minecraft/textures/block/{name}.png");
        let found = entry(&mut archive, &current).or_else(|| {
            earlier.iter().find_map(|path| {
                entry(
                    &mut archive,
                    &format!("assets/minecraft/textures/{path}.png"),
                )
                .or_else(|| entry(&mut archive, &format!("textures/{path}.png")))
            })
        });
        if let Some(rgba) = found.as_deref().and_then(first_frame) {
            textures.push(BlockTexture {
                name: (*name).to_owned(),
                rgba: base64::engine::general_purpose::STANDARD.encode(rgba),
            });
        }
    }
    textures
}

fn entry(archive: &mut zip::ZipArchive<Cursor<Vec<u8>>>, path: &str) -> Option<Vec<u8>> {
    let file = archive.by_name(path).ok()?;
    if file.size() > ENTRY_MAX_BYTES {
        return None;
    }
    let mut bytes = Vec::with_capacity(file.size() as usize);
    file.take(ENTRY_MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    (bytes.len() as u64 <= ENTRY_MAX_BYTES).then_some(bytes)
}

/// The texture's first frame as RGBA: an animated texture stacks its frames down the image.
fn first_frame(png: &[u8]) -> Option<Vec<u8>> {
    let mut decoder = png::Decoder::new_with_limits(
        Cursor::new(png),
        png::Limits {
            bytes: DECODER_BUDGET_BYTES,
        },
    );
    decoder.set_ignore_text_chunk(true);
    decoder.set_ignore_iccp_chunk(true);
    decoder.set_transformations(
        png::Transformations::EXPAND | png::Transformations::ALPHA | png::Transformations::STRIP_16,
    );
    let mut reader = decoder.read_info().ok()?;
    let info = reader.info();
    if info.width != SIDE || info.height < SIDE || info.height > SIDE * 256 {
        return None;
    }
    let mut buffer = vec![0; reader.output_buffer_size()];
    let frame = reader.next_frame(&mut buffer).ok()?;
    let texels = (SIDE * SIDE) as usize;
    match frame.color_type {
        png::ColorType::Rgba => Some(buffer[..texels * 4].to_vec()),
        png::ColorType::GrayscaleAlpha => Some(
            buffer[..texels * 2]
                .chunks_exact(2)
                .flat_map(|texel| [texel[0], texel[0], texel[0], texel[1]])
                .collect(),
        ),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn png(color: png::ColorType, height: u32, data: &[u8]) -> Vec<u8> {
        let mut bytes = Vec::new();
        let mut encoder = png::Encoder::new(&mut bytes, SIDE, height);
        encoder.set_color(color);
        encoder.set_depth(png::BitDepth::Eight);
        if color == png::ColorType::Indexed {
            encoder.set_palette(vec![10, 20, 30, 200, 100, 50]);
            encoder.set_trns(vec![0, 255]);
        }
        encoder
            .write_header()
            .unwrap()
            .write_image_data(data)
            .unwrap();
        bytes
    }

    fn archive(entries: &[(&str, Vec<u8>)]) -> Vec<u8> {
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        for (path, bytes) in entries {
            writer
                .start_file(*path, zip::write::SimpleFileOptions::default())
                .unwrap();
            writer.write_all(bytes).unwrap();
        }
        writer.finish().unwrap().into_inner()
    }

    fn texels(texture: &BlockTexture) -> Vec<u8> {
        base64::engine::general_purpose::STANDARD
            .decode(&texture.rgba)
            .unwrap()
    }

    #[test]
    fn textures_keep_their_current_name_whatever_the_archive_calls_them() {
        let mut palette = vec![1u8; 256];
        palette[0] = 0;
        let strip = [vec![7u8; 256 * 3], vec![9u8; 256 * 3]].concat();
        let textures = extract(archive(&[
            (
                "assets/minecraft/textures/block/dirt.png",
                png(png::ColorType::Indexed, SIDE, &palette),
            ),
            (
                "assets/minecraft/textures/blocks/log_oak.png",
                png(png::ColorType::Grayscale, SIDE, &[128; 256]),
            ),
            (
                "textures/blocks/lava.png",
                png(png::ColorType::Rgb, SIDE * 2, &strip),
            ),
            (
                "assets/minecraft/textures/block/stone.png",
                png(png::ColorType::Rgb, 8, &[0; 16 * 8 * 3]),
            ),
            (
                "assets/minecraft/textures/block/sand.png",
                b"not a png".to_vec(),
            ),
        ]));
        let names: Vec<_> = textures
            .iter()
            .map(|texture| texture.name.as_str())
            .collect();
        assert_eq!(names, ["dirt", "lava_still", "oak_log"]);
        let dirt = texels(&textures[0]);
        assert_eq!(dirt.len(), 1024);
        assert_eq!(&dirt[..8], [10, 20, 30, 0, 200, 100, 50, 255]);
        assert_eq!(&texels(&textures[1])[..4], [7, 7, 7, 255]);
        assert_eq!(&texels(&textures[2])[..4], [128, 128, 128, 255]);
    }

    #[test]
    fn an_archive_without_block_textures_yields_none() {
        assert!(extract(archive(&[("pack.png", vec![1, 2, 3])])).is_empty());
        assert!(extract(b"not an archive".to_vec()).is_empty());
    }
}
