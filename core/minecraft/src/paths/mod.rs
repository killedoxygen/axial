use std::path::{Path, PathBuf};

pub fn assets_dir(mc_dir: &Path) -> PathBuf {
    mc_dir.join("assets")
}

pub fn libraries_dir(mc_dir: &Path) -> PathBuf {
    mc_dir.join("libraries")
}

pub fn versions_dir(mc_dir: &Path) -> PathBuf {
    mc_dir.join("versions")
}

pub fn default_minecraft_dir() -> Option<PathBuf> {
    if cfg!(target_os = "windows") {
        std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .map(|path| path.join(".minecraft"))
    } else if cfg!(target_os = "macos") {
        std::env::var_os("HOME").map(PathBuf::from).map(|path| {
            path.join("Library")
                .join("Application Support")
                .join("minecraft")
        })
    } else {
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .map(|path| path.join(".minecraft"))
    }
}

pub fn runtime_dirs(mc_dir: &Path) -> Vec<PathBuf> {
    let mut dirs = vec![mc_dir.join("runtime")];
    if cfg!(target_os = "windows")
        && let Some(local_app_data) = std::env::var_os("LOCALAPPDATA")
    {
        dirs.push(
            PathBuf::from(local_app_data)
                .join("Packages")
                .join("Microsoft.4297127D64EC6_8wekyb3d8bbwe")
                .join("LocalCache")
                .join("Local")
                .join("runtime"),
        );
    }
    dirs
}

pub fn validate_installation(mc_dir: &Path) -> bool {
    ["versions", "libraries", "assets"]
        .iter()
        .all(|subdir| mc_dir.join(subdir).is_dir())
}

pub fn create_minecraft_dir(dir: &Path) -> std::io::Result<()> {
    for subdir in ["versions", "libraries", "assets"] {
        std::fs::create_dir_all(dir.join(subdir))?;
    }
    Ok(())
}

pub fn is_legacy_assets(mc_dir: &Path, asset_index_id: &str) -> bool {
    let index_path = assets_dir(mc_dir)
        .join("indexes")
        .join(format!("{asset_index_id}.json"));
    let Ok(data) = std::fs::read_to_string(index_path) else {
        return false;
    };

    #[derive(serde::Deserialize)]
    struct AssetIndexFlags {
        #[serde(rename = "virtual", default)]
        virtual_flag: bool,
        #[serde(rename = "map_to_resources", default)]
        map_to_resources: bool,
    }

    let Ok(flags) = serde_json::from_str::<AssetIndexFlags>(&data) else {
        return false;
    };
    flags.virtual_flag || flags.map_to_resources
}
