use std::path::{Path as FsPath, PathBuf};

use super::SKIN_UPLOAD_MAX_BYTES;
use super::image::{is_valid_cape_texture_png, is_valid_normalized_skin_cache_png, texture_key};

pub(super) const PROFILE_SKIN_FILE_CACHE_CONTROL: &str = "private, max-age=300";
pub(super) const PROFILE_CAPE_FILE_CACHE_CONTROL: &str = "private, max-age=86400";
const PROFILE_SKIN_FILE_CACHE_DIR: &str = "profile-cache";
const PROFILE_CAPE_FILE_CACHE_DIR: &str = "cape-cache";

pub(super) fn profile_skin_file_cache_path(skins_dir: &FsPath, texture_url: &str) -> PathBuf {
    skins_dir
        .join(PROFILE_SKIN_FILE_CACHE_DIR)
        .join(format!("{}.png", profile_skin_file_cache_key(texture_url)))
}

pub(super) fn profile_cape_file_cache_path(skins_dir: &FsPath, texture_url: &str) -> PathBuf {
    skins_dir
        .join(PROFILE_CAPE_FILE_CACHE_DIR)
        .join(format!("{}.png", profile_skin_file_cache_key(texture_url)))
}

fn profile_skin_file_cache_key(texture_url: &str) -> String {
    texture_key(texture_url.as_bytes())
}

pub(super) async fn read_profile_skin_file_cache(path: &FsPath) -> Option<Vec<u8>> {
    let metadata = tokio::fs::metadata(path).await.ok()?;
    if !metadata.is_file() || metadata.len() > SKIN_UPLOAD_MAX_BYTES as u64 {
        return None;
    }

    let bytes = tokio::fs::read(path).await.ok()?;
    if bytes.len() > SKIN_UPLOAD_MAX_BYTES || !is_valid_normalized_skin_cache_png(&bytes) {
        return None;
    }

    Some(bytes)
}

pub(super) async fn read_profile_cape_file_cache(path: &FsPath) -> Option<Vec<u8>> {
    let metadata = tokio::fs::metadata(path).await.ok()?;
    if !metadata.is_file() || metadata.len() > SKIN_UPLOAD_MAX_BYTES as u64 {
        return None;
    }

    let bytes = tokio::fs::read(path).await.ok()?;
    if bytes.len() > SKIN_UPLOAD_MAX_BYTES || !is_valid_cape_texture_png(&bytes) {
        return None;
    }

    Some(bytes)
}

pub(super) async fn write_profile_file_cache(
    path: &FsPath,
    bytes: &[u8],
) -> Result<(), std::io::Error> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    tokio::fs::write(path, bytes).await
}
