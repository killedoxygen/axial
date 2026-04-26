use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct LauncherProfiles {
    #[serde(default)]
    pub profiles: HashMap<String, Profile>,
    #[serde(default, rename = "clientToken")]
    pub client_token: String,
    #[serde(default)]
    pub settings: ProfileSettings,
    #[serde(default)]
    pub version: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Profile {
    pub created: String,
    pub icon: String,
    #[serde(rename = "lastUsed")]
    pub last_used: String,
    #[serde(rename = "lastVersionId")]
    pub last_version_id: String,
    pub name: String,
    #[serde(rename = "type")]
    pub kind: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ProfileSettings {}

pub fn ensure_launcher_profiles(mc_dir: &Path, version_id: &str) -> std::io::Result<()> {
    let profiles_path = mc_dir.join("launcher_profiles.json");
    let mut profiles = fs::read_to_string(&profiles_path)
        .ok()
        .and_then(|data| serde_json::from_str::<LauncherProfiles>(&data).ok())
        .unwrap_or_default();

    if profiles.client_token.is_empty() {
        profiles.client_token = generate_client_token();
    }
    if profiles.version == 0 {
        profiles.version = 3;
    }
    if profiles.profiles.is_empty() {
        let now = now_rfc3339();
        profiles.profiles.insert(
            "(Default)".to_string(),
            Profile {
                created: now.clone(),
                icon: "Grass".to_string(),
                last_used: now.clone(),
                last_version_id: "latest-release".to_string(),
                name: "(Default)".to_string(),
                kind: "latest-release".to_string(),
            },
        );
    }
    if !version_id.is_empty() && !profiles.profiles.contains_key(version_id) {
        let now = now_rfc3339();
        profiles.profiles.insert(
            version_id.to_string(),
            Profile {
                created: now.clone(),
                icon: "Furnace".to_string(),
                last_used: now,
                last_version_id: version_id.to_string(),
                name: version_id.to_string(),
                kind: "custom".to_string(),
            },
        );
    }

    let out = serde_json::to_string_pretty(&profiles)?;
    fs::write(&profiles_path, &out)?;

    let store_path = mc_dir.join("launcher_profiles_microsoft_store.json");
    if !store_path.exists() {
        fs::write(store_path, out)?;
    }

    Ok(())
}

fn now_rfc3339() -> String {
    chrono::DateTime::<chrono::Utc>::from(SystemTime::now()).to_rfc3339()
}

fn generate_client_token() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_nanos())
        .unwrap_or_default();
    format!(
        "{:08x}-{:04x}-{:04x}-{:04x}-{:012x}",
        (nanos & 0xffff_ffff) as u32,
        ((nanos >> 32) & 0xffff) as u16,
        ((nanos >> 48) & 0xffff) as u16,
        ((nanos >> 64) & 0xffff) as u16,
        ((nanos >> 80) & 0xffff_ffff_ffff) as u64,
    )
}
