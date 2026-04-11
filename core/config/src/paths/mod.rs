use std::path::PathBuf;

#[derive(Clone, Debug)]
pub struct AppPaths {
    pub config_dir: PathBuf,
    pub config_file: PathBuf,
    pub instances_file: PathBuf,
    pub instances_dir: PathBuf,
    pub music_dir: PathBuf,
}

impl AppPaths {
    pub fn detect() -> Self {
        let config_dir = if cfg!(target_os = "windows") {
            std::env::var_os("APPDATA")
                .map(PathBuf::from)
                .map(|path| path.join("croopor"))
        } else {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .map(|path| path.join(".croopor"))
        }
        .unwrap_or_else(|| PathBuf::from(".croopor"));

        Self {
            config_file: config_dir.join("config.json"),
            instances_file: config_dir.join("instances.json"),
            instances_dir: config_dir.join("instances"),
            music_dir: config_dir.join("music"),
            config_dir,
        }
    }
}
