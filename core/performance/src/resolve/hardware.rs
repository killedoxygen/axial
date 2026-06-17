use crate::types::HardwareProfile;
#[cfg(target_os = "linux")]
use std::fs;
use std::path::Path;
#[cfg(target_os = "linux")]
use std::path::PathBuf;
#[cfg(target_os = "windows")]
use std::process::Command;
use std::sync::OnceLock;
#[cfg(target_os = "windows")]
use std::sync::mpsc;
#[cfg(target_os = "windows")]
use std::time::Duration;
use sysinfo::System;

pub fn detect_hardware() -> HardwareProfile {
    static HARDWARE_PROFILE: OnceLock<HardwareProfile> = OnceLock::new();

    HARDWARE_PROFILE
        .get_or_init(detect_hardware_uncached)
        .clone()
}

fn detect_hardware_uncached() -> HardwareProfile {
    let mut system = System::new();
    system.refresh_memory();

    let total_ram_mb = (system.total_memory() / (1024 * 1024)).min(i32::MAX as u64) as i32;
    let logical_cores = std::thread::available_parallelism()
        .map(|value| value.get() as i32)
        .unwrap_or(1);
    let (gpu_vendor, gpu_arch) = detect_gpu();

    HardwareProfile {
        total_ram_mb,
        logical_cores,
        gpu_vendor,
        gpu_arch,
    }
}

#[cfg(target_os = "linux")]
fn detect_gpu() -> (String, i32) {
    let mut vendors = Vec::new();
    let Ok(entries) = fs::read_dir("/sys/class/drm") else {
        return (String::new(), 0);
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if !is_drm_card_path(&path) {
            continue;
        }

        let Ok(vendor_id) = fs::read_to_string(path.join("device/vendor")) else {
            continue;
        };
        let Some(vendor) = gpu_vendor_from_pci_id(&vendor_id) else {
            continue;
        };

        if vendor == "nvidia" {
            return (vendor.to_string(), detect_nvidia_arch_linux());
        }
        vendors.push(vendor);
    }

    (select_gpu_vendor_from_vendors(vendors), 0)
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(super) fn is_drm_card_path(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|value| value.to_str()) else {
        return false;
    };
    let Some(number) = name.strip_prefix("card") else {
        return false;
    };
    !number.is_empty() && number.chars().all(|character| character.is_ascii_digit())
}

#[cfg(target_os = "linux")]
fn detect_nvidia_arch_linux() -> i32 {
    let Ok(entries) = fs::read_dir("/proc/driver/nvidia/gpus") else {
        return 0;
    };

    entries
        .flatten()
        .filter_map(|entry| nvidia_model_from_information_file(entry.path().join("information")))
        .map(|model| nvidia_arch_from_model(&model))
        .find(|arch| *arch > 0)
        .unwrap_or(0)
}

#[cfg(target_os = "linux")]
pub(super) fn nvidia_model_from_information_file(path: PathBuf) -> Option<String> {
    let contents = fs::read_to_string(path).ok()?;
    nvidia_model_from_information(&contents)
}

#[cfg(target_os = "windows")]
fn detect_gpu() -> (String, i32) {
    let Some(output) = run_windows_gpu_query() else {
        return (String::new(), 0);
    };
    let names = parse_windows_gpu_names(&output);
    select_gpu_from_names(&names)
}

#[cfg(target_os = "windows")]
fn run_windows_gpu_query() -> Option<String> {
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let output = Command::new("wmic")
            .args(["path", "win32_VideoController", "get", "name"])
            .output()
            .ok()
            .and_then(|output| {
                if output.status.success() {
                    decode_windows_command_output(&output.stdout)
                } else {
                    None
                }
            })
            .or_else(|| {
                Command::new("powershell")
                    .args([
                        "-NoProfile",
                        "-Command",
                        "Get-CimInstance Win32_VideoController | Select-Object -ExpandProperty Name",
                    ])
                    .output()
                    .ok()
                    .and_then(|output| {
                        if output.status.success() {
                            decode_windows_command_output(&output.stdout)
                        } else {
                            None
                        }
                    })
            });
        let _ = sender.send(output);
    });

    receiver.recv_timeout(Duration::from_secs(2)).ok().flatten()
}

#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub(super) fn decode_windows_command_output(bytes: &[u8]) -> Option<String> {
    if bytes.is_empty() {
        return None;
    }

    let looks_utf16le = bytes.starts_with(&[0xff, 0xfe])
        || bytes
            .iter()
            .skip(1)
            .step_by(2)
            .take(32)
            .filter(|byte| **byte == 0)
            .count()
            >= 8;
    if looks_utf16le {
        let mut values = Vec::with_capacity(bytes.len() / 2);
        for pair in bytes.chunks_exact(2) {
            values.push(u16::from_le_bytes([pair[0], pair[1]]));
        }
        return String::from_utf16(&values)
            .ok()
            .map(|value| value.trim_start_matches('\u{feff}').to_string());
    }

    String::from_utf8(bytes.to_vec()).ok()
}

#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub(super) fn parse_windows_gpu_names(output: &str) -> Vec<String> {
    output
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.eq_ignore_ascii_case("name"))
        .map(str::to_string)
        .collect()
}

#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub(super) fn select_gpu_from_names(names: &[String]) -> (String, i32) {
    for vendor in ["nvidia", "amd", "intel"] {
        if let Some(name) = names
            .iter()
            .find(|name| gpu_vendor_from_model_name(name) == Some(vendor))
        {
            let arch = if vendor == "nvidia" {
                nvidia_arch_from_model(name)
            } else {
                0
            };
            return (vendor.to_string(), arch);
        }
    }
    (String::new(), 0)
}

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
fn detect_gpu() -> (String, i32) {
    (String::new(), 0)
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(super) fn gpu_vendor_from_pci_id(value: &str) -> Option<&'static str> {
    match value.trim().to_lowercase().as_str() {
        "0x10de" => Some("nvidia"),
        "0x1002" | "0x1022" => Some("amd"),
        "0x8086" => Some("intel"),
        _ => None,
    }
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(super) fn select_gpu_vendor_from_vendors<'a>(
    vendors: impl IntoIterator<Item = &'a str>,
) -> String {
    let mut best = "";
    for vendor in vendors {
        if vendor.eq_ignore_ascii_case("nvidia") {
            return "nvidia".to_string();
        }
        if vendor.eq_ignore_ascii_case("amd") && best != "amd" {
            best = "amd";
        }
        if vendor.eq_ignore_ascii_case("intel") && best.is_empty() {
            best = "intel";
        }
    }
    best.to_string()
}

#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub(super) fn gpu_vendor_from_model_name(value: &str) -> Option<&'static str> {
    let normalized = value.to_lowercase();
    if normalized.contains("nvidia") || normalized.contains("geforce") || normalized.contains("rtx")
    {
        Some("nvidia")
    } else if normalized.contains("amd")
        || normalized.contains("radeon")
        || normalized.contains("advanced micro devices")
    {
        Some("amd")
    } else if normalized.contains("intel") {
        Some("intel")
    } else {
        None
    }
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(super) fn nvidia_model_from_information(contents: &str) -> Option<String> {
    contents.lines().find_map(|line| {
        let (key, value) = line.split_once(':')?;
        if key.trim().eq_ignore_ascii_case("model") {
            let model = value.trim();
            if !model.is_empty() {
                return Some(model.to_string());
            }
        }
        None
    })
}

pub(super) fn nvidia_arch_from_model(model: &str) -> i32 {
    let normalized = model.to_uppercase();
    for (needle, arch) in [
        ("QUADRO RTX", 2),
        ("RTX A", 3),
        ("RTX 50", 4),
        ("RTX 40", 4),
        ("RTX 30", 3),
        ("RTX 20", 2),
        ("GTX 16", 2),
        ("GTX 10", 1),
    ] {
        if normalized.contains(needle) {
            return arch;
        }
    }
    0
}
