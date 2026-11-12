//! Block textures of an installed version, read from its own client archive. The launcher draws its
//! picture of an instance with them, so each version looks like itself.

use super::{artifacts::VersionSummary, queue::InstallError};
use base64::Engine;
use serde::Serialize;
use std::io::{Cursor, Read};
use ts_rs::TS;

/// One side of a block texture, in texels; the sun and moon are twice that.
const SIDE: u32 = 16;
const SKY_SIDE: u32 = 32;
/// A tile of the sheet, across and down.
type Tile = (u32, u32);
/// The sheet that holds every block of a version from before textures were separate files: sixteen tiles a side.
const SHEET: &str = "terrain.png";
const SHEET_SIDE: u32 = SIDE * 16;
const ARCHIVE_MAX_BYTES: u64 = 128 << 20;
const ENTRY_MAX_BYTES: u64 = 256 << 10;
const DECODER_BUDGET_BYTES: usize = 1 << 20;

/// Each texture by its current name, then the names earlier versions gave it as paths under `textures/`, then
/// its tile across and down the sheet of the versions before those.
#[rustfmt::skip]
const TEXTURES: &[(&str, &[&str], Option<Tile>)] = &[
    ("grass_block_top", &["blocks/grass_top"], Some((0, 0))),
    ("grass_block_side", &["blocks/grass_side"], Some((3, 0))),
    ("grass_block_side_overlay", &["blocks/grass_side_overlay"], Some((6, 2))),
    ("grass_block_snow", &["blocks/grass_side_snowed"], Some((4, 4))),
    ("dirt", &["blocks/dirt"], Some((2, 0))),
    ("podzol_top", &["blocks/dirt_podzol_top"], None),
    ("podzol_side", &["blocks/dirt_podzol_side"], None),
    ("stone", &["blocks/stone"], Some((1, 0))),
    ("andesite", &["blocks/stone_andesite"], None),
    ("granite", &["blocks/stone_granite"], None),
    ("coal_ore", &["blocks/coal_ore", "blocks/oreCoal"], Some((2, 2))),
    ("gravel", &["blocks/gravel"], Some((3, 1))),
    ("sand", &["blocks/sand"], Some((2, 1))),
    ("sandstone", &["blocks/sandstone_normal", "blocks/sandstone_side"], Some((0, 12))),
    ("red_sand", &["blocks/red_sand"], None),
    ("snow", &["blocks/snow"], Some((2, 4))),
    ("ice", &["blocks/ice"], Some((3, 4))),
    ("obsidian", &["blocks/obsidian"], Some((5, 2))),
    ("crying_obsidian", &[], None),
    ("netherrack", &["blocks/netherrack", "blocks/hellrock"], Some((7, 6))),
    ("magma", &["blocks/magma"], None),
    ("terracotta", &["blocks/hardened_clay"], None),
    ("orange_terracotta", &["blocks/hardened_clay_stained_orange"], None),
    ("yellow_terracotta", &["blocks/hardened_clay_stained_yellow"], None),
    ("brown_terracotta", &["blocks/hardened_clay_stained_brown"], None),
    ("red_terracotta", &["blocks/hardened_clay_stained_red"], None),
    ("white_terracotta", &["blocks/hardened_clay_stained_white"], None),
    ("light_gray_terracotta", &["blocks/hardened_clay_stained_silver"], None),
    ("lava_still", &["blocks/lava_still", "blocks/lava"], Some((13, 14))),
    ("oak_log", &["blocks/log_oak", "blocks/tree_side"], Some((4, 1))),
    ("oak_log_top", &["blocks/log_oak_top", "blocks/tree_top"], Some((5, 1))),
    ("birch_log", &["blocks/log_birch", "blocks/tree_birch"], Some((5, 7))),
    ("birch_log_top", &["blocks/log_birch_top", "blocks/tree_top"], Some((5, 1))),
    ("spruce_log", &["blocks/log_spruce", "blocks/tree_spruce"], Some((4, 7))),
    ("spruce_log_top", &["blocks/log_spruce_top", "blocks/tree_top"], Some((5, 1))),
    ("dark_oak_log", &["blocks/log_big_oak"], None),
    ("dark_oak_log_top", &["blocks/log_big_oak_top"], None),
    ("acacia_log", &["blocks/log_acacia"], None),
    ("acacia_log_top", &["blocks/log_acacia_top"], None),
    ("cherry_log", &[], None),
    ("cherry_log_top", &[], None),
    ("oak_leaves", &["blocks/leaves_oak", "blocks/leaves"], Some((4, 3))),
    ("birch_leaves", &["blocks/leaves_birch", "blocks/leaves"], Some((4, 3))),
    ("spruce_leaves", &["blocks/leaves_spruce"], Some((4, 8))),
    ("dark_oak_leaves", &["blocks/leaves_big_oak"], None),
    ("acacia_leaves", &["blocks/leaves_acacia"], None),
    ("cherry_leaves", &[], None),
    ("vine", &["blocks/vine"], Some((15, 8))),
    ("cactus_side", &["blocks/cactus_side"], Some((6, 4))),
    ("cactus_top", &["blocks/cactus_top"], Some((5, 4))),
    ("short_grass", &["block/grass", "blocks/tallgrass"], Some((7, 2))),
    ("tall_grass_bottom", &["blocks/double_plant_grass_bottom"], None),
    ("tall_grass_top", &["blocks/double_plant_grass_top"], None),
    ("fern", &["blocks/fern"], Some((8, 3))),
    ("poppy", &["blocks/flower_rose", "blocks/rose"], Some((12, 0))),
    ("dandelion", &["blocks/flower_dandelion", "blocks/flower"], Some((13, 0))),
    ("cornflower", &[], None),
    ("oxeye_daisy", &["blocks/flower_oxeye_daisy"], None),
    ("azure_bluet", &["blocks/flower_houstonia"], None),
    ("allium", &["blocks/flower_allium"], None),
    ("blue_orchid", &["blocks/flower_blue_orchid"], None),
    ("dead_bush", &["blocks/deadbush"], Some((7, 3))),
    ("sugar_cane", &["blocks/reeds"], Some((9, 4))),
    ("red_mushroom", &["blocks/mushroom_red"], Some((12, 1))),
    ("brown_mushroom", &["blocks/mushroom_brown"], Some((13, 1))),
    ("lily_pad", &["blocks/waterlily"], Some((12, 4))),
    ("pink_petals", &[], None),
    ("firefly_bush", &[], None),
    ("firefly_bush_emissive", &[], None),
];

/// Textures with gaps in them. Where a sheet has no such texture yet, the tile holds some solid block instead.
const CUT_OUT: &[&str] = &[
    "grass_block_side_overlay",
    "short_grass",
    "fern",
    "dead_bush",
    "poppy",
    "dandelion",
    "red_mushroom",
    "brown_mushroom",
    "sugar_cane",
    "lily_pad",
    "vine",
];

const FOLIAGE_TILE: Tile = (4, 3);

/// Where the very first sheets kept the few tiles they later moved.
fn earliest(name: &str) -> Option<Tile> {
    match name {
        "oak_leaves" | "birch_leaves" => Some((6, 1)),
        "lava_still" => Some((14, 1)),
        _ => None,
    }
}

/// Whether a sheet's tile holds a texture: not empty, not the purple that fills a sheet's unused tiles, and
/// with gaps if the texture is one that has them.
fn drawn(tile: &[u8], cut_out: bool) -> bool {
    let texels = || tile.chunks_exact(4);
    let filler = texels()
        .filter(|texel| texel[0] > 180 && texel[1] < 160 && texel[2] > 220 && texel[3] == 255)
        .count();
    let clear = texels().filter(|texel| texel[3] == 0).count();
    filler * 2 < SIDE as usize * SIDE as usize
        && clear < SIDE as usize * SIDE as usize
        && (!cut_out || clear > 0)
}

/// The sun and the full moon by the names the picture knows them by, then every path a version has kept them at.
const SKY: &[(&str, &[&str])] = &[
    (
        "sun",
        &[
            "assets/minecraft/textures/environment/celestial/sun.png",
            "assets/minecraft/textures/environment/sun.png",
            "environment/sun.png",
            "terrain/sun.png",
        ],
    ),
    (
        "moon",
        &[
            "assets/minecraft/textures/environment/celestial/moon/full_moon.png",
            "assets/minecraft/textures/environment/moon_phases.png",
            "environment/moon_phases.png",
            "environment/moon.png",
            "terrain/moon_phases.png",
            "terrain/moon.png",
        ],
    ),
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
    /// Square RGBA texels, base64: sixteen a side for a block, thirty-two for the sun and the moon.
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
    let sheet = entry(&mut archive, SHEET)
        .and_then(|png| decode(&png))
        .filter(|(width, height, _)| *width == SHEET_SIDE && *height == SHEET_SIDE);
    let mut textures = Vec::new();
    let named = |name: &str, rgba: Vec<u8>| BlockTexture {
        name: name.to_owned(),
        rgba: base64::engine::general_purpose::STANDARD.encode(rgba),
    };
    for (name, earlier, tile) in TEXTURES {
        let rgba = match (&sheet, tile) {
            (Some((_, _, texels)), Some(tile)) => {
                let tile_at = |(across, down): Tile| {
                    crop(texels, SHEET_SIDE, across * SIDE, down * SIDE, SIDE)
                };
                // The very first sheets have no foliage where it later went; below their third row they
                // hold other blocks than the later ones do.
                let first = !drawn(&tile_at(FOLIAGE_TILE), false);
                let tile = if first {
                    earliest(name).or(Some(*tile).filter(|(_, down)| *down < 3))
                } else {
                    Some(*tile)
                };
                tile.map(tile_at)
                    .filter(|tile| drawn(tile, CUT_OUT.contains(name)))
            }
            (Some(_), None) => None,
            (None, _) => {
                let current = format!("assets/minecraft/textures/block/{name}.png");
                entry(&mut archive, &current)
                    .or_else(|| {
                        earlier.iter().find_map(|path| {
                            entry(
                                &mut archive,
                                &format!("assets/minecraft/textures/{path}.png"),
                            )
                            .or_else(|| entry(&mut archive, &format!("textures/{path}.png")))
                        })
                    })
                    .and_then(|png| decode(&png))
                    // An animated texture stacks its frames down the image; the first is the texture.
                    .filter(|(width, height, _)| *width == SIDE && *height >= SIDE)
                    .map(|(_, _, texels)| crop(&texels, SIDE, 0, 0, SIDE))
            }
        };
        textures.extend(rgba.map(|rgba| named(name, rgba)));
    }
    if textures.is_empty() {
        return textures;
    }
    for (name, paths) in SKY {
        let rgba = paths
            .iter()
            .find_map(|path| entry(&mut archive, path))
            .and_then(|png| decode(&png))
            // The moon's phases share one image, the full moon first.
            .filter(|(width, height, _)| *width >= SKY_SIDE && *height >= SKY_SIDE)
            .map(|(width, _, texels)| crop(&texels, width, 0, 0, SKY_SIDE));
        textures.extend(rgba.map(|rgba| named(name, rgba)));
    }
    textures
}

/// A square of `side` texels out of an RGBA image `width` texels wide.
fn crop(texels: &[u8], width: u32, left: u32, top: u32, side: u32) -> Vec<u8> {
    (top..top + side)
        .flat_map(|row| {
            let start = ((row * width + left) * 4) as usize;
            texels[start..start + (side * 4) as usize].iter().copied()
        })
        .collect()
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

/// An image's width, height and RGBA texels.
fn decode(png: &[u8]) -> Option<(u32, u32, Vec<u8>)> {
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
    let (width, height) = (reader.info().width, reader.info().height);
    if width > SHEET_SIDE || height > SIDE * 256 {
        return None;
    }
    let mut buffer = vec![0; reader.output_buffer_size()];
    let frame = reader.next_frame(&mut buffer).ok()?;
    let texels = (width * height) as usize;
    let rgba = match frame.color_type {
        png::ColorType::Rgba => buffer[..texels * 4].to_vec(),
        png::ColorType::GrayscaleAlpha => buffer[..texels * 2]
            .chunks_exact(2)
            .flat_map(|texel| [texel[0], texel[0], texel[0], texel[1]])
            .collect(),
        _ => return None,
    };
    Some((width, height, rgba))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn png(color: png::ColorType, width: u32, height: u32, data: &[u8]) -> Vec<u8> {
        let mut bytes = Vec::new();
        let mut encoder = png::Encoder::new(&mut bytes, width, height);
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
                png(png::ColorType::Indexed, SIDE, SIDE, &palette),
            ),
            (
                "assets/minecraft/textures/blocks/log_oak.png",
                png(png::ColorType::Grayscale, SIDE, SIDE, &[128; 256]),
            ),
            (
                "textures/blocks/lava.png",
                png(png::ColorType::Rgb, SIDE, SIDE * 2, &strip),
            ),
            (
                "assets/minecraft/textures/block/stone.png",
                png(png::ColorType::Rgb, SIDE, 8, &[0; 16 * 8 * 3]),
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

    /// A sheet whose tiles are each one flat colour, by tile across and down.
    fn sheet(tile: impl Fn(u32, u32) -> [u8; 4]) -> Vec<u8> {
        let mut data = Vec::new();
        for y in 0..SHEET_SIDE {
            for x in 0..SHEET_SIDE {
                data.extend(tile(x / SIDE, y / SIDE));
            }
        }
        png(png::ColorType::Rgba, SHEET_SIDE, SHEET_SIDE, &data)
    }

    const FILLER: [u8; 4] = [214, 127, 255, 255];

    #[test]
    fn a_sheet_gives_the_tiles_it_has_and_none_of_its_filler() {
        let textures = extract(archive(&[
            (
                SHEET,
                sheet(|across, down| match (across, down) {
                    (2, 0) => [134, 96, 67, 255],
                    (4, 3) => [60, 120, 40, 255],
                    // A solid block where a later sheet keeps a plant.
                    (7, 2) => [250, 220, 60, 255],
                    (12, 0) => [0, 0, 0, 0],
                    _ => FILLER,
                }),
            ),
            (
                "terrain/moon.png",
                png(png::ColorType::Rgba, SKY_SIDE, SKY_SIDE, &[9; 32 * 32 * 4]),
            ),
        ]));
        let names: Vec<_> = textures
            .iter()
            .map(|texture| texture.name.as_str())
            .collect();
        assert_eq!(names, ["dirt", "oak_leaves", "birch_leaves", "moon"]);
        assert_eq!(&texels(&textures[0])[..4], [134, 96, 67, 255]);
        assert_eq!(texels(&textures[3]).len(), 32 * 32 * 4);
    }

    #[test]
    fn the_first_sheets_keep_foliage_elsewhere_and_other_blocks_below() {
        let textures = extract(archive(&[(
            SHEET,
            sheet(|across, down| match (across, down) {
                (2, 0) => [134, 96, 67, 255],
                (6, 1) => [60, 160, 40, 255],
                // Wool, where later sheets keep snow.
                (2, 4) => [230, 40, 40, 255],
                _ => FILLER,
            }),
        )]));
        let names: Vec<_> = textures
            .iter()
            .map(|texture| texture.name.as_str())
            .collect();
        assert_eq!(names, ["dirt", "oak_leaves", "birch_leaves"]);
        assert_eq!(&texels(&textures[1])[..4], [60, 160, 40, 255]);
    }

    #[test]
    fn the_full_moon_is_cut_from_the_image_of_its_phases() {
        let mut phases = vec![0u8; 128 * 64 * 4];
        for row in 0..32 {
            for texel in 0..32 {
                phases[(row * 128 + texel) * 4] = 200;
            }
        }
        let textures = extract(archive(&[
            (
                "assets/minecraft/textures/block/dirt.png",
                png(png::ColorType::Rgba, SIDE, SIDE, &[5; 1024]),
            ),
            (
                "assets/minecraft/textures/environment/moon_phases.png",
                png(png::ColorType::Rgba, 128, 64, &phases),
            ),
        ]));
        let moon = texels(&textures[1]);
        assert_eq!(textures[1].name, "moon");
        assert_eq!(moon.len(), 32 * 32 * 4);
        assert!(moon.chunks_exact(4).all(|texel| texel[0] == 200));
    }
}
