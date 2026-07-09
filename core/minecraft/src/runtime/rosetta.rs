use super::model::JavaRuntimeLookupError;
use std::io::Read;
use std::path::Path;

const CPU_TYPE_ARM64: u32 = 0x0100_000c;
const MACH_O_HEADER_READ_LIMIT: u64 = 4096;
const MAX_FAT_ARCHES: usize = 64;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum MachOArm64Compatibility {
    HasArm64Slice,
    LacksArm64Slice,
    UnknownCompatible,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum RosettaRuntimeDecision {
    Compatible,
    RosettaRequired,
}

#[derive(Clone, Copy)]
enum Endian {
    Big,
    Little,
}

pub(super) fn rosetta_required_error_for_current_host(
    java_path: &Path,
    component: &str,
) -> Option<JavaRuntimeLookupError> {
    if !current_host_is_apple_silicon_macos() {
        return None;
    }

    // installed with Rosetta, checked uncached so installing Rosetta then
    // retrying works without restarting Croopor
    if rosetta_present_for_current_host() {
        return None;
    }

    let binary = mach_o_arm64_compatibility_for_path(java_path);
    if rosetta_requirement_for_managed_runtime(
        std::env::consts::OS,
        std::env::consts::ARCH,
        false,
        binary,
    ) != RosettaRuntimeDecision::RosettaRequired
    {
        return None;
    }

    Some(JavaRuntimeLookupError::RosettaRequired {
        component: component.to_string(),
    })
}

pub(super) fn rosetta_requirement_for_managed_runtime(
    host_os: &str,
    host_arch: &str,
    rosetta_present: bool,
    binary: MachOArm64Compatibility,
) -> RosettaRuntimeDecision {
    if host_os != "macos" || host_arch != "aarch64" || rosetta_present {
        return RosettaRuntimeDecision::Compatible;
    }

    match binary {
        MachOArm64Compatibility::LacksArm64Slice => RosettaRuntimeDecision::RosettaRequired,
        MachOArm64Compatibility::HasArm64Slice | MachOArm64Compatibility::UnknownCompatible => {
            RosettaRuntimeDecision::Compatible
        }
    }
}

pub(super) fn parse_mach_o_arm64_compatibility(bytes: &[u8]) -> MachOArm64Compatibility {
    let Some(magic) = bytes.get(0..4) else {
        return MachOArm64Compatibility::UnknownCompatible;
    };

    if matches_magic(magic, 0xfeed_facf, Endian::Big)
        || matches_magic(magic, 0xfeed_face, Endian::Big)
    {
        return parse_thin_mach_o(bytes, Endian::Big);
    }
    if matches_magic(magic, 0xfeed_facf, Endian::Little)
        || matches_magic(magic, 0xfeed_face, Endian::Little)
    {
        return parse_thin_mach_o(bytes, Endian::Little);
    }
    if matches_magic(magic, 0xcafe_babe, Endian::Big) {
        return parse_fat_mach_o(bytes, Endian::Big, 20);
    }
    if matches_magic(magic, 0xcafe_babe, Endian::Little) {
        return parse_fat_mach_o(bytes, Endian::Little, 20);
    }
    if matches_magic(magic, 0xcafe_babf, Endian::Big) {
        return parse_fat_mach_o(bytes, Endian::Big, 32);
    }
    if matches_magic(magic, 0xcafe_babf, Endian::Little) {
        return parse_fat_mach_o(bytes, Endian::Little, 32);
    }

    MachOArm64Compatibility::UnknownCompatible
}

pub(super) fn is_rosetta_exec_error(error: &std::io::Error) -> bool {
    #[cfg(target_os = "macos")]
    {
        const ENOEXEC: i32 = 8;
        const EBADARCH: i32 = 86;
        matches!(error.raw_os_error(), Some(ENOEXEC | EBADARCH))
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = error;
        false
    }
}

fn mach_o_arm64_compatibility_for_path(path: &Path) -> MachOArm64Compatibility {
    let Ok(file) = std::fs::File::open(path) else {
        return MachOArm64Compatibility::UnknownCompatible;
    };
    let mut bytes = Vec::new();
    if file
        .take(MACH_O_HEADER_READ_LIMIT)
        .read_to_end(&mut bytes)
        .is_err()
    {
        return MachOArm64Compatibility::UnknownCompatible;
    }
    parse_mach_o_arm64_compatibility(&bytes)
}

fn parse_thin_mach_o(bytes: &[u8], endian: Endian) -> MachOArm64Compatibility {
    let Some(cputype) = read_u32(bytes, 4, endian) else {
        return MachOArm64Compatibility::UnknownCompatible;
    };
    if cputype == CPU_TYPE_ARM64 {
        MachOArm64Compatibility::HasArm64Slice
    } else {
        MachOArm64Compatibility::LacksArm64Slice
    }
}

fn parse_fat_mach_o(
    bytes: &[u8],
    endian: Endian,
    arch_entry_size: usize,
) -> MachOArm64Compatibility {
    let Some(count) = read_u32(bytes, 4, endian) else {
        return MachOArm64Compatibility::UnknownCompatible;
    };
    let Ok(count) = usize::try_from(count) else {
        return MachOArm64Compatibility::UnknownCompatible;
    };
    if count == 0 || count > MAX_FAT_ARCHES {
        return MachOArm64Compatibility::UnknownCompatible;
    }
    let Some(table_len) = arch_entry_size
        .checked_mul(count)
        .and_then(|size| size.checked_add(8))
    else {
        return MachOArm64Compatibility::UnknownCompatible;
    };
    if bytes.len() < table_len {
        return MachOArm64Compatibility::UnknownCompatible;
    }

    for index in 0..count {
        let offset = 8 + index * arch_entry_size;
        let Some(cputype) = read_u32(bytes, offset, endian) else {
            return MachOArm64Compatibility::UnknownCompatible;
        };
        if cputype == CPU_TYPE_ARM64 {
            return MachOArm64Compatibility::HasArm64Slice;
        }
    }

    MachOArm64Compatibility::LacksArm64Slice
}

fn read_u32(bytes: &[u8], offset: usize, endian: Endian) -> Option<u32> {
    let raw = bytes.get(offset..offset.checked_add(4)?)?;
    let raw: [u8; 4] = raw.try_into().ok()?;
    Some(match endian {
        Endian::Big => u32::from_be_bytes(raw),
        Endian::Little => u32::from_le_bytes(raw),
    })
}

fn matches_magic(bytes: &[u8], magic: u32, endian: Endian) -> bool {
    let expected = match endian {
        Endian::Big => magic.to_be_bytes(),
        Endian::Little => magic.to_le_bytes(),
    };
    bytes == expected
}

fn current_host_is_apple_silicon_macos() -> bool {
    cfg!(all(target_os = "macos", target_arch = "aarch64"))
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
fn rosetta_present_for_current_host() -> bool {
    std::fs::metadata("/Library/Apple/usr/share/rosetta/rosetta").is_ok()
}

#[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
fn rosetta_present_for_current_host() -> bool {
    false
}
