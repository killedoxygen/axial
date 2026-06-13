use croopor_config::AppPaths;
use serde::{Deserialize, Serialize};
use std::{
    fs, io,
    path::{Path, PathBuf},
    sync::Mutex,
};

const SKIN_STORE_SCHEMA: &str = "croopor.skins.saved";
const SKIN_STORE_SCHEMA_VERSION: u32 = 3;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SavedSkinRecord {
    pub texture_key: String,
    pub name: String,
    pub variant: String,
    pub source: String,
    pub cape_id: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub applied_at: Option<String>,
    pub byte_size: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SavedSkinDeleteResult {
    Deleted(SavedSkinRecord),
    Applied,
    Missing,
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
        cape_id: Option<String>,
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
            cape_id,
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
        match self.delete_record(texture_key, false)? {
            SavedSkinDeleteResult::Deleted(record) => Ok(Some(record)),
            SavedSkinDeleteResult::Applied | SavedSkinDeleteResult::Missing => Ok(None),
        }
    }

    pub fn delete_unapplied(&self, texture_key: &str) -> io::Result<SavedSkinDeleteResult> {
        self.delete_record(texture_key, true)
    }

    fn delete_record(
        &self,
        texture_key: &str,
        protect_applied: bool,
    ) -> io::Result<SavedSkinDeleteResult> {
        let _guard = self.lock.lock().map_err(|_| lock_error())?;
        let mut index = self.load_index()?;
        let Some(position) = index
            .skins
            .iter()
            .position(|skin| skin.texture_key == texture_key)
        else {
            return Ok(SavedSkinDeleteResult::Missing);
        };

        if protect_applied && index.skins[position].applied_at.is_some() {
            return Ok(SavedSkinDeleteResult::Applied);
        }

        let record = index.skins.remove(position);
        let file_path = self.skin_file_path(texture_key);
        let parked_file_path = self.skin_delete_file_path(texture_key);
        let parked_file = park_file_for_delete(&file_path, &parked_file_path)?;
        if let Err(error) = self.persist_index(&index) {
            if parked_file {
                let _ = restore_parked_file(&parked_file_path, &file_path);
            }
            return Err(error);
        }
        if parked_file {
            let _ = fs::remove_file(&parked_file_path);
        }

        Ok(SavedSkinDeleteResult::Deleted(record))
    }

    pub fn update_metadata(
        &self,
        texture_key: &str,
        name: Option<String>,
        variant: Option<String>,
        cape_id: Option<Option<String>>,
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
        let applied_profile_changed = record.applied_at.is_some()
            && (variant
                .as_ref()
                .is_some_and(|variant| variant != &record.variant)
                || cape_id
                    .as_ref()
                    .is_some_and(|cape_id| cape_id != &record.cape_id));
        if let Some(name) = name {
            record.name = name;
        }
        if let Some(variant) = variant {
            record.variant = variant;
        }
        if let Some(cape_id) = cape_id {
            record.cape_id = cape_id;
        }
        if applied_profile_changed {
            record.applied_at = None;
        }
        record.updated_at = chrono::Utc::now().to_rfc3339();
        let updated = record.clone();
        self.persist_index(&index)?;

        Ok(Some(updated))
    }

    pub fn replace_texture(
        &self,
        texture_key: &str,
        new_texture_key: String,
        name: String,
        variant: String,
        cape_id: Option<String>,
        png_bytes: &[u8],
    ) -> io::Result<Option<SavedSkinRecord>> {
        let _guard = self.lock.lock().map_err(|_| lock_error())?;
        fs::create_dir_all(&self.file_dir)?;
        let mut index = self.load_index()?;
        let Some(position) = index
            .skins
            .iter()
            .position(|skin| skin.texture_key == texture_key)
        else {
            return Ok(None);
        };

        let old_record = index.skins[position].clone();
        let same_texture = old_record.texture_key == new_texture_key;
        let applied_profile_changed =
            old_record.variant != variant || old_record.cape_id != cape_id || !same_texture;
        let now = chrono::Utc::now().to_rfc3339();
        let mut record = SavedSkinRecord {
            texture_key: new_texture_key.clone(),
            name,
            variant,
            source: old_record.source.clone(),
            cape_id,
            created_at: old_record.created_at.clone(),
            updated_at: now,
            applied_at: if applied_profile_changed {
                None
            } else {
                old_record.applied_at.clone()
            },
            byte_size: png_bytes.len(),
        };

        let new_file_path = self.skin_file_path(&new_texture_key);
        let new_file_existed = new_file_path.exists();
        write_atomic(&new_file_path, png_bytes)?;

        if same_texture {
            index.skins[position] = record.clone();
        } else if let Some(existing_position) = index
            .skins
            .iter()
            .position(|skin| skin.texture_key == new_texture_key)
        {
            let existing_applied_at = index.skins[existing_position].applied_at.clone();
            record.applied_at = if index.skins[existing_position].variant == record.variant
                && index.skins[existing_position].cape_id == record.cape_id
            {
                existing_applied_at
            } else {
                None
            };
            index.skins[existing_position] = record.clone();
            index.skins.remove(position);
        } else {
            index.skins[position] = record.clone();
        }

        if let Err(error) = self.persist_index(&index) {
            if !new_file_existed {
                let _ = fs::remove_file(&new_file_path);
            }
            return Err(error);
        }

        if !same_texture {
            let old_file_path = self.skin_file_path(&old_record.texture_key);
            if old_file_path != new_file_path {
                let _ = fs::remove_file(old_file_path);
            }
        }

        Ok(Some(record))
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

    pub fn clear_applied(&self) -> io::Result<()> {
        let _guard = self.lock.lock().map_err(|_| lock_error())?;
        let mut index = self.load_index()?;
        if !index.skins.iter().any(|skin| skin.applied_at.is_some()) {
            return Ok(());
        }

        for skin in &mut index.skins {
            skin.applied_at = None;
        }
        self.persist_index(&index)
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

    fn skin_delete_file_path(&self, texture_key: &str) -> PathBuf {
        self.root_dir
            .join("pending-delete")
            .join(format!("{texture_key}.png"))
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

fn park_file_for_delete(source: &Path, parked: &Path) -> io::Result<bool> {
    let Some(parent) = parked.parent() else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "saved skin delete path has no parent",
        ));
    };
    fs::create_dir_all(parent)?;
    match fs::remove_file(parked) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    match fs::rename(source, parked) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

fn restore_parked_file(parked: &Path, destination: &Path) -> io::Result<()> {
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::rename(parked, destination)
}

fn replace_file(source: &Path, destination: &Path) -> io::Result<()> {
    let first_error = match fs::rename(source, destination) {
        Ok(()) => return Ok(()),
        Err(error) => error,
    };
    if !source.exists() {
        return Err(first_error);
    }
    if destination.exists() && !destination.is_dir() {
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
    io::Error::other("saved skin store lock poisoned")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn replace_file_preserves_existing_destination_when_source_is_missing() {
        let root = test_root("missing-source");
        fs::create_dir_all(&root).expect("create test root");
        let source = root.join("skin.tmp");
        let destination = root.join("skin.png");
        fs::write(&destination, b"existing").expect("write destination");

        let error = replace_file(&source, &destination).expect_err("replace should fail");

        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        assert_eq!(
            fs::read(&destination).expect("destination should remain readable"),
            b"existing"
        );
        assert!(!source.exists());

        cleanup(&root);
    }

    #[test]
    fn replace_file_preserves_directory_destination_on_failed_promotion() {
        let root = test_root("directory-destination");
        fs::create_dir_all(&root).expect("create test root");
        let source = root.join("skin.tmp");
        let destination = root.join("skin.png");
        fs::write(&source, b"replacement").expect("write source");
        fs::create_dir(&destination).expect("create destination directory");

        replace_file(&source, &destination).expect_err("replace should fail");

        assert!(destination.is_dir());
        assert!(!source.exists());

        cleanup(&root);
    }

    #[test]
    fn saved_skin_delete_restores_png_when_index_persistence_fails() {
        let root = test_root("delete-persist-failure");
        let store = test_store(&root);
        let texture_key = "saved-skin";
        let png_bytes = b"skin bytes";
        let saved = store
            .save(
                texture_key.to_string(),
                "Saved Skin".to_string(),
                "classic".to_string(),
                "test".to_string(),
                None,
                png_bytes,
            )
            .expect("save skin");
        fs::create_dir(store.index_path.with_extension("tmp")).expect("block index temp path");

        store
            .delete(texture_key)
            .expect_err("delete should fail when index cannot persist");

        assert_eq!(
            store
                .list()
                .expect("list should still load persisted index"),
            vec![saved]
        );
        assert_eq!(
            store
                .read_png(texture_key)
                .expect("read should not fail")
                .expect("skin png should still be indexed and readable"),
            png_bytes
        );
        assert!(!store.skin_delete_file_path(texture_key).exists());

        cleanup(&root);
    }

    #[test]
    fn saved_skin_delete_unapplied_preserves_applied_record() {
        let root = test_root("delete-applied-preserved");
        let store = test_store(&root);
        let texture_key = "saved-skin";
        let png_bytes = b"skin bytes";
        let saved = store
            .save(
                texture_key.to_string(),
                "Saved Skin".to_string(),
                "classic".to_string(),
                "test".to_string(),
                None,
                png_bytes,
            )
            .expect("save skin");
        store
            .mark_applied(texture_key)
            .expect("mark applied")
            .expect("skin should exist");

        let result = store
            .delete_unapplied(texture_key)
            .expect("delete should return a protected result");

        assert_eq!(result, SavedSkinDeleteResult::Applied);
        let listed = store.list().expect("list saved skins");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].texture_key, saved.texture_key);
        assert!(listed[0].applied_at.is_some());
        assert_eq!(
            store
                .read_png(texture_key)
                .expect("read should not fail")
                .expect("skin png should remain readable"),
            png_bytes
        );

        cleanup(&root);
    }

    #[test]
    fn saved_skin_name_update_preserves_applied_marker() {
        let root = test_root("name-update-preserves-applied");
        let store = test_store(&root);
        let texture_key = "saved-skin";
        store
            .save(
                texture_key.to_string(),
                "Saved Skin".to_string(),
                "classic".to_string(),
                "test".to_string(),
                Some("cape-one".to_string()),
                b"skin bytes",
            )
            .expect("save skin");
        let applied_at = store
            .mark_applied(texture_key)
            .expect("mark applied")
            .expect("applied timestamp");

        let updated = store
            .update_metadata(texture_key, Some("Renamed Skin".to_string()), None, None)
            .expect("update metadata")
            .expect("saved skin should exist");

        assert_eq!(updated.name, "Renamed Skin");
        assert_eq!(updated.applied_at.as_deref(), Some(applied_at.as_str()));

        cleanup(&root);
    }

    #[test]
    fn saved_skin_profile_metadata_update_clears_applied_marker() {
        let root = test_root("profile-metadata-clears-applied");
        let store = test_store(&root);
        let texture_key = "saved-skin";
        store
            .save(
                texture_key.to_string(),
                "Saved Skin".to_string(),
                "classic".to_string(),
                "test".to_string(),
                Some("cape-one".to_string()),
                b"skin bytes",
            )
            .expect("save skin");
        store
            .mark_applied(texture_key)
            .expect("mark applied")
            .expect("applied timestamp");

        let updated = store
            .update_metadata(
                texture_key,
                None,
                Some("slim".to_string()),
                Some(Some("cape-two".to_string())),
            )
            .expect("update metadata")
            .expect("saved skin should exist");

        assert_eq!(updated.variant, "slim");
        assert_eq!(updated.cape_id.as_deref(), Some("cape-two"));
        assert_eq!(updated.applied_at, None);

        cleanup(&root);
    }

    #[test]
    fn saved_skin_same_texture_replacement_clears_applied_marker_when_profile_metadata_changes() {
        let root = test_root("same-texture-replace-clears-applied");
        let store = test_store(&root);
        let texture_key = "saved-skin";
        let png_bytes = b"skin bytes";
        store
            .save(
                texture_key.to_string(),
                "Saved Skin".to_string(),
                "classic".to_string(),
                "test".to_string(),
                None,
                png_bytes,
            )
            .expect("save skin");
        store
            .mark_applied(texture_key)
            .expect("mark applied")
            .expect("applied timestamp");

        let updated = store
            .replace_texture(
                texture_key,
                texture_key.to_string(),
                "Saved Skin".to_string(),
                "slim".to_string(),
                None,
                png_bytes,
            )
            .expect("replace texture")
            .expect("saved skin should exist");

        assert_eq!(updated.texture_key, texture_key);
        assert_eq!(updated.variant, "slim");
        assert_eq!(updated.applied_at, None);

        cleanup(&root);
    }

    fn test_store(root: &Path) -> SavedSkinStore {
        let root_dir = root.join("skins");
        SavedSkinStore {
            file_dir: root_dir.join("files"),
            index_path: root_dir.join("index.json"),
            root_dir,
            lock: Mutex::new(()),
        }
    }

    fn test_root(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be after unix epoch")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "croopor-skins-{name}-{}-{nanos}",
            std::process::id()
        ))
    }

    fn cleanup(root: &Path) {
        let _ = fs::remove_dir_all(root);
    }
}
