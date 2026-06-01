use croopor_config::AppPaths;
use serde::{Deserialize, Serialize};
use std::{
    fs, io,
    path::{Path, PathBuf},
    sync::Mutex,
};

const SKIN_STORE_SCHEMA: &str = "croopor.skins.saved";
const SKIN_STORE_SCHEMA_VERSION: u32 = 2;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SavedSkinRecord {
    pub texture_key: String,
    pub name: String,
    pub variant: String,
    pub source: String,
    pub created_at: String,
    pub updated_at: String,
    pub applied_at: Option<String>,
    pub byte_size: usize,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SavedSkinIndex {
    schema: String,
    schema_version: u32,
    skins: Vec<SavedSkinRecord>,
}

pub struct SavedSkinStore {
    root_dir: PathBuf,
    file_dir: PathBuf,
    index_path: PathBuf,
    lock: Mutex<()>,
}

impl SavedSkinStore {
    pub fn load_from_paths(paths: &AppPaths) -> Self {
        let root_dir = paths.config_dir.join("skins");
        let file_dir = root_dir.join("files");
        let index_path = root_dir.join("index.json");

        Self {
            root_dir,
            file_dir,
            index_path,
            lock: Mutex::new(()),
        }
    }

    pub fn list(&self) -> io::Result<Vec<SavedSkinRecord>> {
        let _guard = self.lock.lock().map_err(|_| lock_error())?;
        let mut skins = self.load_index()?.skins;
        skins.sort_by(|left, right| {
            right
                .updated_at
                .cmp(&left.updated_at)
                .then_with(|| left.name.cmp(&right.name))
                .then_with(|| left.texture_key.cmp(&right.texture_key))
        });
        Ok(skins)
    }

    pub fn save(
        &self,
        texture_key: String,
        name: String,
        variant: String,
        source: String,
        png_bytes: &[u8],
    ) -> io::Result<SavedSkinRecord> {
        let _guard = self.lock.lock().map_err(|_| lock_error())?;
        fs::create_dir_all(&self.file_dir)?;

        let mut index = self.load_index()?;
        let now = chrono::Utc::now().to_rfc3339();
        let existing = index
            .skins
            .iter()
            .position(|skin| skin.texture_key == texture_key);
        let created_at = existing
            .and_then(|position| index.skins.get(position))
            .map(|skin| skin.created_at.clone())
            .unwrap_or_else(|| now.clone());
        let applied_at = existing
            .and_then(|position| index.skins.get(position))
            .and_then(|skin| skin.applied_at.clone());

        let record = SavedSkinRecord {
            texture_key,
            name,
            variant,
            source,
            created_at,
            updated_at: now,
            applied_at,
            byte_size: png_bytes.len(),
        };

        write_atomic(&self.skin_file_path(&record.texture_key), png_bytes)?;
        if let Some(index_position) = existing {
            index.skins[index_position] = record.clone();
        } else {
            index.skins.push(record.clone());
        }
        self.persist_index(&index)?;

        Ok(record)
    }

    pub fn delete(&self, texture_key: &str) -> io::Result<Option<SavedSkinRecord>> {
        let _guard = self.lock.lock().map_err(|_| lock_error())?;
        let mut index = self.load_index()?;
        let Some(position) = index
            .skins
            .iter()
            .position(|skin| skin.texture_key == texture_key)
        else {
            return Ok(None);
        };

        let record = index.skins.remove(position);
        let file_path = self.skin_file_path(texture_key);
        match fs::remove_file(&file_path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        self.persist_index(&index)?;

        Ok(Some(record))
    }

    pub fn update_metadata(
        &self,
        texture_key: &str,
        name: Option<String>,
        variant: Option<String>,
    ) -> io::Result<Option<SavedSkinRecord>> {
        let _guard = self.lock.lock().map_err(|_| lock_error())?;
        let mut index = self.load_index()?;
        let Some(position) = index
            .skins
            .iter()
            .position(|skin| skin.texture_key == texture_key)
        else {
            return Ok(None);
        };

        let record = &mut index.skins[position];
        if let Some(name) = name {
            record.name = name;
        }
        if let Some(variant) = variant {
            record.variant = variant;
        }
        record.updated_at = chrono::Utc::now().to_rfc3339();
        let updated = record.clone();
        self.persist_index(&index)?;

        Ok(Some(updated))
    }

    pub fn mark_applied(&self, texture_key: &str) -> io::Result<Option<String>> {
        let _guard = self.lock.lock().map_err(|_| lock_error())?;
        let mut index = self.load_index()?;
        if !index
            .skins
            .iter()
            .any(|skin| skin.texture_key == texture_key)
        {
            return Ok(None);
        }

        let applied_at = chrono::Utc::now().to_rfc3339();
        for skin in &mut index.skins {
            skin.applied_at = if skin.texture_key == texture_key {
                Some(applied_at.clone())
            } else {
                None
            };
        }
        self.persist_index(&index)?;

        Ok(Some(applied_at))
    }

    pub fn read_png(&self, texture_key: &str) -> io::Result<Option<Vec<u8>>> {
        let _guard = self.lock.lock().map_err(|_| lock_error())?;
        let index = self.load_index()?;
        if !index
            .skins
            .iter()
            .any(|skin| skin.texture_key == texture_key)
        {
            return Ok(None);
        }

        match fs::read(self.skin_file_path(texture_key)) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    fn load_index(&self) -> io::Result<SavedSkinIndex> {
        match fs::read_to_string(&self.index_path) {
            Ok(data) => {
                let index = serde_json::from_str::<SavedSkinIndex>(&data)
                    .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
                if index.schema != SKIN_STORE_SCHEMA
                    || index.schema_version != SKIN_STORE_SCHEMA_VERSION
                {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "unsupported saved skin index schema",
                    ));
                }
                Ok(index)
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(empty_index()),
            Err(error) => Err(error),
        }
    }

    fn persist_index(&self, index: &SavedSkinIndex) -> io::Result<()> {
        fs::create_dir_all(&self.root_dir)?;
        let data = serde_json::to_vec_pretty(index)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        write_atomic(&self.index_path, &data)
    }

    fn skin_file_path(&self, texture_key: &str) -> PathBuf {
        self.file_dir.join(format!("{texture_key}.png"))
    }
}

fn empty_index() -> SavedSkinIndex {
    SavedSkinIndex {
        schema: SKIN_STORE_SCHEMA.to_string(),
        schema_version: SKIN_STORE_SCHEMA_VERSION,
        skins: Vec::new(),
    }
}

fn write_atomic(path: &Path, data: &[u8]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let temp_path = path.with_extension("tmp");
    fs::write(&temp_path, data)?;
    replace_file(&temp_path, path)
}

fn replace_file(source: &Path, destination: &Path) -> io::Result<()> {
    if fs::rename(source, destination).is_ok() {
        return Ok(());
    }
    if destination.exists() {
        let _ = fs::remove_file(destination);
    }
    match fs::rename(source, destination) {
        Ok(()) => Ok(()),
        Err(error) => {
            let _ = fs::remove_file(source);
            Err(error)
        }
    }
}

fn lock_error() -> io::Error {
    io::Error::new(io::ErrorKind::Other, "saved skin store lock poisoned")
}
