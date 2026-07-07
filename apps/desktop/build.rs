use serde_json::Value;
use std::{env, fs};

const DEV_TAURI_CONFIG: &str = "tauri.dev.conf.json";
const DEV_ICON_PNG: &str = "icons/dev/icon.png";
const DEV_ICON_ICO: &str = "icons/dev/icon.ico";

fn main() {
    apply_dev_tauri_config();
    tauri_build::build()
}

fn apply_dev_tauri_config() {
    println!("cargo:rerun-if-env-changed=PROFILE");
    println!("cargo:rerun-if-env-changed=TAURI_CONFIG");

    if env::var("PROFILE").as_deref() == Ok("release") {
        return;
    }

    println!("cargo:rerun-if-changed={DEV_TAURI_CONFIG}");
    println!("cargo:rerun-if-changed={DEV_ICON_PNG}");
    println!("cargo:rerun-if-changed={DEV_ICON_ICO}");

    let mut config = env::var("TAURI_CONFIG")
        .ok()
        .map(|raw| serde_json::from_str::<Value>(&raw).expect("TAURI_CONFIG must be valid JSON"))
        .unwrap_or_else(|| Value::Object(Default::default()));
    let dev_config = fs::read_to_string(DEV_TAURI_CONFIG).expect("failed to read dev Tauri config");
    let dev_config: Value =
        serde_json::from_str(&dev_config).expect("dev Tauri config must be valid JSON");

    merge_json(&mut config, dev_config);
    let config = serde_json::to_string(&config).expect("failed to encode dev Tauri config");

    println!("cargo:rustc-env=TAURI_CONFIG={config}");
    // SAFETY: this build script does not spawn threads before Tauri reads the
    // process environment, and the override is scoped to this build process.
    unsafe {
        env::set_var("TAURI_CONFIG", config);
    }
}

fn merge_json(base: &mut Value, patch: Value) {
    match (base, patch) {
        (Value::Object(base), Value::Object(patch)) => {
            for (key, value) in patch {
                merge_json(base.entry(key).or_insert(Value::Null), value);
            }
        }
        (base, patch) => *base = patch,
    }
}
