use crate::types::CompositionState;
use std::fs;
use std::path::{Path, PathBuf};
use thiserror::Error;

const LOCK_FILE_NAME: &str = ".croopor-lock.json";

#[derive(Debug, Error)]
pub enum StateError {
    #[error("failed to read state: {0}")]
    Read(#[from] std::io::Error),
    #[error("failed to parse state: {0}")]
    Parse(#[from] serde_json::Error),
}

pub fn load_state(instance_mods_dir: &Path) -> Result<Option<CompositionState>, StateError> {
    let path = lock_file_path(instance_mods_dir);
    match fs::read_to_string(path) {
        Ok(data) => Ok(Some(serde_json::from_str(&data)?)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(StateError::Read(error)),
    }
}

pub fn save_state(instance_mods_dir: &Path, state: &CompositionState) -> Result<(), StateError> {
    fs::create_dir_all(instance_mods_dir)?;
    let data = serde_json::to_string_pretty(state)?;
    let path = lock_file_path(instance_mods_dir);
    let temp_path = path.with_extension("json.tmp");
    fs::write(&temp_path, data)?;
    replace_file_atomic(&temp_path, &path)?;
    Ok(())
}

pub fn remove_state(instance_mods_dir: &Path) -> Result<(), StateError> {
    let path = lock_file_path(instance_mods_dir);
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(StateError::Read(error)),
    }
}

pub fn lock_file_path(instance_mods_dir: &Path) -> PathBuf {
    instance_mods_dir.join(LOCK_FILE_NAME)
}

fn replace_file_atomic(temp_path: &Path, final_path: &Path) -> Result<(), std::io::Error> {
    if fs::rename(temp_path, final_path).is_ok() {
        return Ok(());
    }

    if final_path.exists() {
        let _ = fs::remove_file(final_path);
    }

    match fs::rename(temp_path, final_path) {
        Ok(()) => Ok(()),
        Err(error) => {
            let _ = fs::remove_file(temp_path);
            Err(error)
        }
    }
}
