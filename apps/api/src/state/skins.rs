use crate::execution::anchored_record::{
    AnchoredRecordDirectory, AnchoredRecordObservation, AnchoredRecordRetirement,
    AnchoredRecordTarget, AnchoredRecordWriteOutcome,
};
use axial_config::AppRootSession;
use axial_fs::{Directory, DirectoryListingState, EffectOwner, EntryKind};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use std::collections::HashSet;
use std::ffi::OsStr;
use std::io;
use std::sync::{Arc, Mutex};

const SKIN_STORE_SCHEMA: &str = "axial.skins.saved";
const SKIN_STORE_SCHEMA_VERSION: u32 = 3;
const SKIN_INDEX_NAME: &str = "index.json";
const SKIN_INDEX_MAX_BYTES: u64 = 16 * 1024 * 1024;
const SKIN_INDEX_MAX_RECORDS: usize = 32_768;
// This keeps restart authentication bounded while remaining well above a practical wardrobe.
const SKIN_INDEX_MAX_TOTAL_PNG_BYTES: u64 = 512 * 1024 * 1024;
const SKIN_FILE_MAX_ENTRIES: usize = SKIN_INDEX_MAX_RECORDS * 2;
const SAVED_SKIN_NAME_MAX_CHARS: usize = 64;
const SAVED_SKIN_SOURCE_MAX_BYTES: usize = 64;
const SAVED_SKIN_CAPE_ID_MAX_BYTES: usize = 80;
const SAVED_SKIN_TIMESTAMP_MAX_BYTES: usize = 64;
pub const SAVED_SKIN_PNG_MAX_BYTES: usize = 256 * 1024;

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
    pending_file_retirements: Mutex<Vec<PendingSavedSkinFileRetirement>>,
    index_effects: EffectOwner,
    file_effects: EffectOwner,
    index_target: AnchoredRecordTarget,
    files_root: Directory,
    index_directory: AnchoredRecordDirectory,
    files_directory: AnchoredRecordDirectory,
    mutation: Mutex<()>,
    #[cfg(test)]
    fail_next_index_write: std::sync::atomic::AtomicBool,
    #[cfg(test)]
    defer_next_file_retirement: std::sync::atomic::AtomicBool,
}

struct PendingSavedSkinFileRetirement {
    texture_key: String,
    retirement: Option<AnchoredRecordRetirement>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ContentAddressedPngPublication {
    Created,
    Existing,
}

impl SavedSkinStore {
    pub(crate) fn claim(root_session: Arc<AppRootSession>) -> io::Result<Self> {
        let (index_root, files_root) = root_session.prepare_saved_skin_directories()?;
        let index_directory =
            AnchoredRecordDirectory::from_directory(Arc::clone(&root_session), index_root);
        let files_directory =
            AnchoredRecordDirectory::from_directory(root_session, files_root.clone());
        let index_target =
            index_directory.target(OsStr::new(SKIN_INDEX_NAME), SKIN_INDEX_MAX_BYTES)?;
        let store = Self {
            pending_file_retirements: Mutex::new(Vec::new()),
            index_effects: index_directory.effect_owner()?,
            file_effects: files_directory.effect_owner()?,
            index_target,
            files_root,
            index_directory,
            files_directory,
            mutation: Mutex::new(()),
            #[cfg(test)]
            fail_next_index_write: std::sync::atomic::AtomicBool::new(false),
            #[cfg(test)]
            defer_next_file_retirement: std::sync::atomic::AtomicBool::new(false),
        };
        let index = store.load_index()?;
        store.reconcile_file_inventory(&index)?;
        Ok(store)
    }

    pub fn list(&self) -> io::Result<Vec<SavedSkinRecord>> {
        let _mutation = self.lock_mutation()?;
        self.retry_pending_file_retirements();
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

    pub(crate) async fn settle_retirements_for_shutdown(self: &Arc<Self>) -> io::Result<()> {
        let store = Arc::clone(self);
        tokio::task::spawn_blocking(move || store.settle_retirements_blocking())
            .await
            .map_err(|error| {
                io::Error::other(format!(
                    "saved skin retirement shutdown task failed: {error}"
                ))
            })?
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
        validate_texture_key(&texture_key)?;
        validate_png_identity(&texture_key, png_bytes)?;
        let _mutation = self.lock_mutation()?;
        self.retry_pending_file_retirements();
        self.require_retirement_settled(&texture_key)?;

        let mut index = self.load_index()?;
        let now = chrono::Utc::now().to_rfc3339();
        let existing = index
            .skins
            .iter()
            .position(|skin| skin.texture_key == texture_key);
        let texture_was_indexed = existing.is_some();
        let existing_record = existing.and_then(|position| index.skins.get(position));
        let created_at = existing_record
            .map(|skin| skin.created_at.clone())
            .unwrap_or_else(|| now.clone());
        let applied_at = existing_record.and_then(|skin| {
            (skin.variant == variant && skin.cape_id == cape_id)
                .then(|| skin.applied_at.clone())
                .flatten()
        });
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
        validate_record(&record, io::ErrorKind::InvalidInput)?;

        let file_target = self.file_target(&record.texture_key)?;
        let publication =
            self.publish_content_addressed_png(&file_target, &record.texture_key, png_bytes)?;
        if let Some(index_position) = existing {
            index.skins[index_position] = record.clone();
        } else {
            index.skins.push(record.clone());
        }
        if let Err(error) = self.persist_index(&index) {
            self.retire_new_unindexed_png_after_index_failure(
                &record.texture_key,
                publication,
                texture_was_indexed,
            );
            return Err(error);
        }
        Ok(record)
    }

    pub fn delete_unapplied(&self, texture_key: &str) -> io::Result<SavedSkinDeleteResult> {
        self.delete_record(texture_key)
    }

    fn delete_record(&self, texture_key: &str) -> io::Result<SavedSkinDeleteResult> {
        validate_texture_key(texture_key)?;
        let _mutation = self.lock_mutation()?;
        self.retry_pending_file_retirements();
        let mut index = self.load_index()?;
        let Some(position) = index
            .skins
            .iter()
            .position(|skin| skin.texture_key == texture_key)
        else {
            return Ok(SavedSkinDeleteResult::Missing);
        };
        if index.skins[position].applied_at.is_some() {
            return Ok(SavedSkinDeleteResult::Applied);
        }

        let record = index.skins.remove(position);
        self.persist_index(&index)?;
        self.schedule_file_retirement(texture_key);
        Ok(SavedSkinDeleteResult::Deleted(record))
    }

    pub fn update_metadata(
        &self,
        texture_key: &str,
        name: Option<String>,
        variant: Option<String>,
        cape_id: Option<Option<String>>,
    ) -> io::Result<Option<SavedSkinRecord>> {
        validate_texture_key(texture_key)?;
        let _mutation = self.lock_mutation()?;
        self.retry_pending_file_retirements();
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
        validate_record(record, io::ErrorKind::InvalidInput)?;
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
        validate_texture_key(texture_key)?;
        validate_texture_key(&new_texture_key)?;
        validate_png_identity(&new_texture_key, png_bytes)?;
        let _mutation = self.lock_mutation()?;
        self.retry_pending_file_retirements();
        self.require_retirement_settled(&new_texture_key)?;
        let mut index = self.load_index()?;
        let Some(position) = index
            .skins
            .iter()
            .position(|skin| skin.texture_key == texture_key)
        else {
            return Ok(None);
        };

        let old_record = index.skins[position].clone();
        let new_texture_was_indexed = index
            .skins
            .iter()
            .any(|skin| skin.texture_key == new_texture_key);
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
        validate_record(&record, io::ErrorKind::InvalidInput)?;

        let new_target = self.file_target(&new_texture_key)?;
        let publication =
            self.publish_content_addressed_png(&new_target, &new_texture_key, png_bytes)?;
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
            self.retire_new_unindexed_png_after_index_failure(
                &new_texture_key,
                publication,
                new_texture_was_indexed,
            );
            return Err(error);
        }
        if !same_texture {
            self.schedule_file_retirement(&old_record.texture_key);
        }
        Ok(Some(record))
    }

    pub fn mark_applied(&self, texture_key: &str) -> io::Result<Option<String>> {
        validate_texture_key(texture_key)?;
        let _mutation = self.lock_mutation()?;
        self.retry_pending_file_retirements();
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
        let _mutation = self.lock_mutation()?;
        self.retry_pending_file_retirements();
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
        validate_texture_key(texture_key)?;
        let _mutation = self.lock_mutation()?;
        self.retry_pending_file_retirements();
        let index = self.load_index()?;
        let Some(record) = index
            .skins
            .iter()
            .find(|skin| skin.texture_key == texture_key)
        else {
            return Ok(None);
        };
        let bytes = self.read_file_bytes(texture_key)?.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "saved skin index references a missing PNG",
            )
        })?;
        if bytes.len() != record.byte_size {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "saved skin PNG size does not match its index",
            ));
        }
        validate_png_identity(texture_key, &bytes)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))?;
        Ok(Some(bytes))
    }

    fn load_index(&self) -> io::Result<SavedSkinIndex> {
        let observation = match self
            .index_directory
            .read(OsStr::new(SKIN_INDEX_NAME), SKIN_INDEX_MAX_BYTES)
        {
            Ok(observation) => observation,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(empty_index()),
            Err(error) => return Err(error),
        };
        let bytes = observation.bytes().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "saved skin index exceeds its byte bound",
            )
        })?;
        let index = serde_json::from_slice::<SavedSkinIndex>(bytes)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        validate_index(&index)?;
        observation.admit(SKIN_INDEX_MAX_BYTES)?;
        Ok(index)
    }

    fn persist_index(&self, index: &SavedSkinIndex) -> io::Result<()> {
        validate_index(index)?;
        let data = serde_json::to_vec_pretty(index)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        #[cfg(test)]
        if self
            .fail_next_index_write
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            return Err(io::Error::other("injected saved skin index write failure"));
        }
        self.index_target.write(&self.index_effects, &data)
    }

    fn file_target(&self, texture_key: &str) -> io::Result<AnchoredRecordTarget> {
        validate_texture_key(texture_key)?;
        self.files_directory.target(
            OsStr::new(&format!("{texture_key}.png")),
            SAVED_SKIN_PNG_MAX_BYTES as u64,
        )
    }

    fn read_file_bytes(&self, texture_key: &str) -> io::Result<Option<Vec<u8>>> {
        let name = format!("{texture_key}.png");
        match self
            .files_directory
            .read(OsStr::new(&name), SAVED_SKIN_PNG_MAX_BYTES as u64)
        {
            Ok(observation @ AnchoredRecordObservation::Bytes { .. }) => {
                let bytes = observation
                    .bytes()
                    .expect("bounded saved skin PNG observation has bytes")
                    .to_vec();
                observation.admit(SAVED_SKIN_PNG_MAX_BYTES as u64)?;
                Ok(Some(bytes))
            }
            Ok(AnchoredRecordObservation::Oversized { .. }) => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "saved skin PNG exceeds its byte bound",
            )),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    fn publish_content_addressed_png(
        &self,
        target: &AnchoredRecordTarget,
        texture_key: &str,
        png_bytes: &[u8],
    ) -> io::Result<ContentAddressedPngPublication> {
        if let Some(current) = self.read_file_bytes(texture_key)? {
            validate_png_identity(texture_key, &current)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))?;
            if current != png_bytes {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "saved skin content address resolves to conflicting bytes",
                ));
            }
            return Ok(ContentAddressedPngPublication::Existing);
        }
        match target.write_with_outcome(&self.file_effects, png_bytes)? {
            AnchoredRecordWriteOutcome::Published => Ok(ContentAddressedPngPublication::Created),
            AnchoredRecordWriteOutcome::Existing => Ok(ContentAddressedPngPublication::Existing),
        }
    }

    fn retire_new_unindexed_png_after_index_failure(
        &self,
        texture_key: &str,
        publication: ContentAddressedPngPublication,
        texture_was_indexed: bool,
    ) {
        if publication != ContentAddressedPngPublication::Created || texture_was_indexed {
            return;
        }
        let known_unindexed = self.load_index().is_ok_and(|index| {
            !index
                .skins
                .iter()
                .any(|skin| skin.texture_key == texture_key)
        });
        if known_unindexed {
            self.schedule_file_retirement(texture_key);
        }
    }

    fn reconcile_file_inventory(&self, index: &SavedSkinIndex) -> io::Result<()> {
        let listing = self
            .files_root
            .entries(SKIN_FILE_MAX_ENTRIES.saturating_add(1))?;
        if listing.state() != DirectoryListingState::Complete
            || listing.entries().len() > SKIN_FILE_MAX_ENTRIES
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "saved skin file inventory exceeds its entry bound",
            ));
        }
        let expected = index
            .skins
            .iter()
            .map(|record| (record.texture_key.as_str(), record.byte_size))
            .collect::<std::collections::HashMap<_, _>>();
        let mut observed = HashSet::with_capacity(listing.entries().len());
        for entry in listing.entries() {
            let name = entry.utf8_name().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "saved skin inventory contains a non-UTF-8 entry",
                )
            })?;
            if is_owned_stage_name(name) {
                if entry.kind() != EntryKind::File {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "saved skin inventory contains an invalid stage residue",
                    ));
                }
                self.files_directory
                    .target(OsStr::new(name), SAVED_SKIN_PNG_MAX_BYTES as u64)?
                    .remove(&self.file_effects)?;
                continue;
            }
            let texture_key = canonical_png_texture_key(name)?;
            if entry.kind() != EntryKind::File || !observed.insert(texture_key) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "saved skin inventory contains an invalid or aliased entry",
                ));
            }
            if let Some(expected_size) = expected.get(texture_key) {
                let bytes = self.read_file_bytes(texture_key)?.ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "saved skin inventory changed during reconciliation",
                    )
                })?;
                if bytes.len() != *expected_size {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "saved skin PNG size does not match its index",
                    ));
                }
                validate_png_identity(texture_key, &bytes).map_err(|error| {
                    io::Error::new(io::ErrorKind::InvalidData, error.to_string())
                })?;
            } else {
                self.file_target(texture_key)?.remove(&self.file_effects)?;
            }
        }
        if expected.keys().any(|key| !observed.contains(key)) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "saved skin index references a missing PNG",
            ));
        }
        Ok(())
    }

    fn schedule_file_retirement(&self, texture_key: &str) {
        {
            let mut pending = self
                .pending_file_retirements
                .lock()
                .expect("saved skin retirement lock poisoned");
            if !pending
                .iter()
                .any(|retirement| retirement.texture_key == texture_key)
            {
                pending.push(PendingSavedSkinFileRetirement {
                    texture_key: texture_key.to_string(),
                    retirement: None,
                });
            }
        }
        #[cfg(test)]
        if self
            .defer_next_file_retirement
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            return;
        }
        self.retry_pending_file_retirements();
    }

    fn retry_pending_file_retirements(&self) {
        let pending = std::mem::take(
            &mut *self
                .pending_file_retirements
                .lock()
                .expect("saved skin retirement lock poisoned"),
        );
        let mut unresolved = Vec::new();
        for retirement in pending {
            if let Some(retirement) = self.retry_file_retirement(retirement) {
                unresolved.push(retirement);
            }
        }
        self.pending_file_retirements
            .lock()
            .expect("saved skin retirement lock poisoned")
            .extend(unresolved);
    }

    fn retry_file_retirement(
        &self,
        mut pending: PendingSavedSkinFileRetirement,
    ) -> Option<PendingSavedSkinFileRetirement> {
        let result = if let Some(retirement) = pending.retirement.take() {
            retirement.retry()
        } else {
            let name = format!("{}.png", pending.texture_key);
            let observation = match self
                .files_directory
                .read(OsStr::new(&name), SAVED_SKIN_PNG_MAX_BYTES as u64)
            {
                Ok(observation) => observation,
                Err(error) if error.kind() == io::ErrorKind::NotFound => return None,
                Err(_) => return Some(pending),
            };
            observation.retire(SAVED_SKIN_PNG_MAX_BYTES as u64)
        };
        match result {
            Ok(()) => {
                let name = format!("{}.png", pending.texture_key);
                match self
                    .files_directory
                    .read(OsStr::new(&name), SAVED_SKIN_PNG_MAX_BYTES as u64)
                {
                    Err(error) if error.kind() == io::ErrorKind::NotFound => None,
                    Ok(_) | Err(_) => Some(pending),
                }
            }
            Err(failure) => {
                let (_, retirement) = failure.into_parts();
                pending.retirement = retirement;
                Some(pending)
            }
        }
    }

    fn require_retirement_settled(&self, texture_key: &str) -> io::Result<()> {
        if self
            .pending_file_retirements
            .lock()
            .expect("saved skin retirement lock poisoned")
            .iter()
            .any(|retirement| retirement.texture_key == texture_key)
        {
            Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "saved skin PNG retirement remains unsettled",
            ))
        } else {
            Ok(())
        }
    }

    fn settle_retirements_blocking(&self) -> io::Result<()> {
        let _mutation = self.lock_mutation()?;
        self.retry_pending_file_retirements();
        if self
            .pending_file_retirements
            .lock()
            .expect("saved skin retirement lock poisoned")
            .is_empty()
        {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "saved skin PNG retirement remains unsettled",
            ))
        }
    }

    fn lock_mutation(&self) -> io::Result<std::sync::MutexGuard<'_, ()>> {
        self.mutation
            .lock()
            .map_err(|_| io::Error::other("saved skin store lock poisoned"))
    }

    #[cfg(test)]
    fn inject_next_index_write_failure(&self) {
        self.fail_next_index_write
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    #[cfg(test)]
    fn inject_deferred_file_retirement(&self) {
        self.defer_next_file_retirement
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

impl Drop for SavedSkinStore {
    fn drop(&mut self) {
        self.retry_pending_file_retirements();
        if !self
            .pending_file_retirements
            .lock()
            .expect("saved skin retirement lock poisoned")
            .is_empty()
        {
            std::process::abort();
        }
    }
}

fn empty_index() -> SavedSkinIndex {
    SavedSkinIndex {
        schema: SKIN_STORE_SCHEMA.to_string(),
        schema_version: SKIN_STORE_SCHEMA_VERSION,
        skins: Vec::new(),
    }
}

fn validate_index(index: &SavedSkinIndex) -> io::Result<()> {
    if index.schema != SKIN_STORE_SCHEMA || index.schema_version != SKIN_STORE_SCHEMA_VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unsupported saved skin index schema",
        ));
    }
    if index.skins.len() > SKIN_INDEX_MAX_RECORDS {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "saved skin index exceeds its record bound",
        ));
    }
    let mut texture_keys = HashSet::with_capacity(index.skins.len());
    let mut aggregate_png_bytes = 0_u64;
    for skin in &index.skins {
        validate_record(skin, io::ErrorKind::InvalidData)?;
        let byte_size = u64::try_from(skin.byte_size).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "saved skin PNG size does not fit its persisted bound",
            )
        })?;
        aggregate_png_bytes = aggregate_png_bytes.checked_add(byte_size).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "saved skin index PNG byte budget overflowed",
            )
        })?;
        if aggregate_png_bytes > SKIN_INDEX_MAX_TOTAL_PNG_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "saved skin index exceeds its aggregate PNG byte bound",
            ));
        }
        if !texture_keys.insert(skin.texture_key.as_str()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "saved skin index contains duplicate texture keys",
            ));
        }
    }
    Ok(())
}

fn validate_record(record: &SavedSkinRecord, error_kind: io::ErrorKind) -> io::Result<()> {
    validate_texture_key(&record.texture_key)
        .map_err(|error| io::Error::new(error_kind, error.to_string()))?;
    let valid_name = !record.name.is_empty()
        && record.name.trim() == record.name
        && record.name.chars().count() <= SAVED_SKIN_NAME_MAX_CHARS
        && !record
            .name
            .chars()
            .any(|value| value.is_control() || matches!(value, '/' | '\\'));
    if !valid_name {
        return Err(io::Error::new(error_kind, "saved skin name is invalid"));
    }
    if !matches!(record.variant.as_str(), "classic" | "slim") {
        return Err(io::Error::new(error_kind, "saved skin variant is invalid"));
    }
    if record.source.is_empty()
        || record.source.len() > SAVED_SKIN_SOURCE_MAX_BYTES
        || !record
            .source
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        return Err(io::Error::new(error_kind, "saved skin source is invalid"));
    }
    if record.cape_id.as_ref().is_some_and(|cape_id| {
        cape_id.is_empty()
            || cape_id.len() > SAVED_SKIN_CAPE_ID_MAX_BYTES
            || !cape_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    }) {
        return Err(io::Error::new(error_kind, "saved skin cape id is invalid"));
    }
    for timestamp in [
        Some(record.created_at.as_str()),
        Some(record.updated_at.as_str()),
        record.applied_at.as_deref(),
    ]
    .into_iter()
    .flatten()
    {
        if timestamp.len() > SAVED_SKIN_TIMESTAMP_MAX_BYTES
            || chrono::DateTime::parse_from_rfc3339(timestamp).is_err()
        {
            return Err(io::Error::new(
                error_kind,
                "saved skin timestamp is invalid",
            ));
        }
    }
    if record.byte_size == 0 || record.byte_size > SAVED_SKIN_PNG_MAX_BYTES {
        return Err(io::Error::new(error_kind, "saved skin PNG size is invalid"));
    }
    Ok(())
}

fn validate_texture_key(texture_key: &str) -> io::Result<()> {
    if texture_key.len() == 64
        && texture_key
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "saved skin texture key is invalid",
        ))
    }
}

fn validate_png_identity(texture_key: &str, bytes: &[u8]) -> io::Result<()> {
    validate_png_size(bytes)?;
    if format!("{:x}", Sha256::digest(bytes)) != texture_key {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "saved skin PNG does not match its content address",
        ))
    } else {
        Ok(())
    }
}

fn validate_png_size(bytes: &[u8]) -> io::Result<()> {
    if bytes.len() > SAVED_SKIN_PNG_MAX_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "saved skin PNG exceeds its byte bound",
        ));
    }
    Ok(())
}

fn canonical_png_texture_key(name: &str) -> io::Result<&str> {
    let texture_key = name.strip_suffix(".png").ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "saved skin inventory contains an unknown entry",
        )
    })?;
    validate_texture_key(texture_key)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))?;
    Ok(texture_key)
}

fn is_owned_stage_name(name: &str) -> bool {
    name.strip_prefix(".axial-stage-").is_some_and(|nonce| {
        nonce.len() == 32
            && nonce
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axial_config::AppPaths;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn failed_index_publication_retires_new_png_online() {
        let mut fixture = SkinStoreFixture::new("failed-index-publication");
        let bytes = b"skin bytes";
        let texture_key = texture_key(bytes);
        fixture.store().inject_next_index_write_failure();

        fixture
            .store()
            .save(
                texture_key.clone(),
                "Saved Skin".to_string(),
                "classic".to_string(),
                "test".to_string(),
                None,
                bytes,
            )
            .expect_err("index publication failure");

        assert!(fixture.store().list().expect("list skins").is_empty());
        assert_eq!(
            fixture
                .store()
                .read_file_bytes(&texture_key)
                .expect("inspect published PNG"),
            None
        );
        fixture.restart().expect("restart reconciliation");
        assert!(fixture.store().list().expect("list skins").is_empty());
        assert_eq!(
            fixture
                .store()
                .read_file_bytes(&texture_key)
                .expect("inspect reconciled inventory"),
            None
        );
    }

    #[test]
    fn repeated_failed_index_publications_do_not_accumulate_pngs_online() {
        let fixture = SkinStoreFixture::new("repeated-failed-index-publication");

        for attempt in 0..32 {
            let bytes = format!("skin bytes {attempt}");
            let texture_key = texture_key(bytes.as_bytes());
            fixture.store().inject_next_index_write_failure();

            fixture
                .store()
                .save(
                    texture_key.clone(),
                    "Saved Skin".to_string(),
                    "classic".to_string(),
                    "test".to_string(),
                    None,
                    bytes.as_bytes(),
                )
                .expect_err("index publication failure");

            assert_eq!(
                fixture
                    .store()
                    .read_file_bytes(&texture_key)
                    .expect("inspect failed publication"),
                None
            );
        }

        assert!(fixture.store().list().expect("list skins").is_empty());
        let inventory = fixture
            .store()
            .files_root
            .entries(SKIN_FILE_MAX_ENTRIES)
            .expect("list skin inventory");
        assert_eq!(inventory.state(), DirectoryListingState::Complete);
        assert!(inventory.entries().is_empty());
    }

    #[test]
    fn failed_existing_texture_update_preserves_shared_png() {
        let fixture = SkinStoreFixture::new("failed-existing-texture-update");
        let bytes = b"skin bytes";
        let texture_key = texture_key(bytes);
        fixture.save(bytes);
        fixture.store().inject_next_index_write_failure();

        fixture
            .store()
            .save(
                texture_key.clone(),
                "Renamed".to_string(),
                "classic".to_string(),
                "test".to_string(),
                None,
                bytes,
            )
            .expect_err("index publication failure");

        assert_eq!(
            fixture
                .store()
                .read_png(&texture_key)
                .expect("read shared PNG")
                .expect("shared PNG remains"),
            bytes
        );
        assert_eq!(
            fixture.store().list().expect("list skins")[0].name,
            "Saved Skin"
        );
    }

    #[test]
    fn failed_index_publication_preserves_preexisting_unindexed_png() {
        let mut fixture = SkinStoreFixture::new("failed-preexisting-unindexed-png");
        let bytes = b"skin bytes";
        let texture_key = texture_key(bytes);
        fixture
            .store()
            .file_target(&texture_key)
            .expect("skin target")
            .write(&fixture.store().file_effects, bytes)
            .expect("publish preexisting PNG");
        fixture.store().inject_next_index_write_failure();

        fixture
            .store()
            .save(
                texture_key.clone(),
                "Saved Skin".to_string(),
                "classic".to_string(),
                "test".to_string(),
                None,
                bytes,
            )
            .expect_err("index publication failure");

        assert_eq!(
            fixture
                .store()
                .read_file_bytes(&texture_key)
                .expect("inspect preexisting PNG"),
            Some(bytes.to_vec())
        );
        fixture.restart().expect("restart reconciliation");
        assert_eq!(
            fixture
                .store()
                .read_file_bytes(&texture_key)
                .expect("inspect reconciled inventory"),
            None
        );
    }

    #[test]
    fn repeated_failed_texture_replacements_do_not_accumulate_pngs_online() {
        let fixture = SkinStoreFixture::new("repeated-failed-texture-replacement");
        let original = b"original skin bytes";
        let original_texture_key = texture_key(original);
        fixture.save(original);

        for attempt in 0..32 {
            let replacement = format!("replacement skin bytes {attempt}");
            let replacement_texture_key = texture_key(replacement.as_bytes());
            fixture.store().inject_next_index_write_failure();

            fixture
                .store()
                .replace_texture(
                    &original_texture_key,
                    replacement_texture_key.clone(),
                    "Replacement".to_string(),
                    "slim".to_string(),
                    None,
                    replacement.as_bytes(),
                )
                .expect_err("index publication failure");

            assert_eq!(
                fixture
                    .store()
                    .read_file_bytes(&replacement_texture_key)
                    .expect("inspect failed replacement"),
                None
            );
        }

        assert_eq!(
            fixture
                .store()
                .read_png(&original_texture_key)
                .expect("read original PNG")
                .expect("original PNG remains"),
            original
        );
        let inventory = fixture
            .store()
            .files_root
            .entries(SKIN_FILE_MAX_ENTRIES)
            .expect("list skin inventory");
        assert_eq!(inventory.state(), DirectoryListingState::Complete);
        assert_eq!(inventory.entries().len(), 1);
    }

    #[test]
    fn restart_reconciliation_removes_index_first_cleanup_orphan() {
        let mut fixture = SkinStoreFixture::new("index-first-cleanup");
        let bytes = b"skin bytes";
        let texture_key = texture_key(bytes);
        fixture.save(bytes);
        fixture
            .store()
            .persist_index(&empty_index())
            .expect("publish authoritative empty index");
        assert_eq!(
            fixture
                .store()
                .read_file_bytes(&texture_key)
                .expect("inspect cleanup orphan"),
            Some(bytes.to_vec())
        );

        fixture.restart().expect("restart reconciliation");

        assert!(fixture.store().list().expect("list skins").is_empty());
        assert_eq!(
            fixture
                .store()
                .read_file_bytes(&texture_key)
                .expect("inspect reconciled inventory"),
            None
        );
    }

    #[test]
    fn missing_indexed_png_is_invalid_at_read_and_restart() {
        let mut fixture = SkinStoreFixture::new("missing-indexed-png");
        let bytes = b"skin bytes";
        let texture_key = texture_key(bytes);
        fixture.save(bytes);
        fixture
            .store()
            .file_target(&texture_key)
            .expect("skin target")
            .remove(&fixture.store().file_effects)
            .expect("remove indexed PNG");

        let error = fixture
            .store()
            .read_png(&texture_key)
            .expect_err("missing indexed PNG rejected");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        let error = fixture
            .restart()
            .expect_err("restart rejects missing indexed PNG");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn digest_mismatched_indexed_png_is_invalid_at_restart() {
        let mut fixture = SkinStoreFixture::new("mismatched-indexed-png");
        let bytes = b"skin bytes";
        let texture_key = texture_key(bytes);
        fixture.save(bytes);
        drop(fixture.store.take());
        fs::write(
            fixture
                .root
                .join("skins/files")
                .join(format!("{texture_key}.png")),
            b"bad content",
        )
        .expect("replace fixture PNG");

        let error = fixture
            .restart()
            .expect_err("restart rejects mismatched indexed PNG");

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn stale_index_and_png_observations_cannot_be_admitted() {
        let fixture = SkinStoreFixture::new("stale-observation-admission");
        let bytes = b"skin bytes";
        let texture_key = texture_key(bytes);
        fixture.save(bytes);
        let index_observation = fixture
            .store()
            .index_directory
            .read(OsStr::new(SKIN_INDEX_NAME), SKIN_INDEX_MAX_BYTES)
            .expect("observe index generation");
        let png_name = format!("{texture_key}.png");
        let png_observation = fixture
            .store()
            .files_directory
            .read(OsStr::new(&png_name), SAVED_SKIN_PNG_MAX_BYTES as u64)
            .expect("observe PNG generation");

        fs::write(
            fixture.root.join("skins/index.json"),
            b"changed index generation",
        )
        .expect("replace index generation");
        fs::write(
            fixture.root.join("skins/files").join(&png_name),
            b"changed PNG generation",
        )
        .expect("replace PNG generation");

        assert!(
            index_observation.admit(SKIN_INDEX_MAX_BYTES).is_err(),
            "stale index observation must not become mutation authority"
        );
        assert!(
            png_observation
                .admit(SAVED_SKIN_PNG_MAX_BYTES as u64)
                .is_err(),
            "stale PNG observation must not become mutation authority"
        );
    }

    #[test]
    fn startup_removes_exact_owned_stage_residue() {
        let root = test_root("owned-stage-residue");
        let files = root.join("skins/files");
        fs::create_dir_all(&files).expect("create files root");
        let stage_name = ".axial-stage-0123456789abcdef0123456789abcdef";
        fs::write(files.join(stage_name), b"incomplete").expect("write stage residue");

        let store = claim_store(&root).expect("claim reconciled skin store");

        assert!(store.list().expect("list skins").is_empty());
        assert!(!files.join(stage_name).exists());
        drop(store);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn startup_rejects_noncanonical_and_wrong_kind_inventory_entries() {
        for name in [
            format!("{}.PNG", "a".repeat(64)),
            format!("{}.png", "A".repeat(64)),
            format!(".axial-stage-{}g", "0".repeat(31)),
        ] {
            let root = test_root("noncanonical-inventory");
            let files = root.join("skins/files");
            fs::create_dir_all(&files).expect("create files root");
            fs::write(files.join(name), b"unknown").expect("write unknown entry");

            let error = claim_store(&root)
                .err()
                .expect("noncanonical inventory rejected");

            assert_eq!(error.kind(), io::ErrorKind::InvalidData);
            let _ = fs::remove_dir_all(root);
        }

        let root = test_root("wrong-kind-inventory");
        let files = root.join("skins/files");
        fs::create_dir_all(files.join(format!("{}.png", "a".repeat(64))))
            .expect("create wrong-kind entry");
        let error = claim_store(&root)
            .err()
            .expect("wrong-kind inventory rejected");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn delete_unapplied_preserves_applied_record_and_png() {
        let fixture = SkinStoreFixture::new("delete-applied-preserved");
        let bytes = b"skin bytes";
        let texture_key = texture_key(bytes);
        fixture.save(bytes);
        fixture
            .store()
            .mark_applied(&texture_key)
            .expect("mark applied")
            .expect("skin exists");

        assert_eq!(
            fixture
                .store()
                .delete_unapplied(&texture_key)
                .expect("protected delete"),
            SavedSkinDeleteResult::Applied
        );
        assert_eq!(
            fixture
                .store()
                .read_png(&texture_key)
                .expect("read PNG")
                .expect("PNG exists"),
            bytes
        );
    }

    #[test]
    fn committed_delete_retries_deferred_png_retirement_online() {
        let fixture = SkinStoreFixture::new("delete-online-retirement");
        let bytes = b"skin bytes";
        let texture_key = texture_key(bytes);
        fixture.save(bytes);
        fixture.store().inject_deferred_file_retirement();

        assert!(matches!(
            fixture
                .store()
                .delete_unapplied(&texture_key)
                .expect("commit logical delete"),
            SavedSkinDeleteResult::Deleted(_)
        ));
        assert_eq!(
            fixture
                .store()
                .read_file_bytes(&texture_key)
                .expect("inspect deferred PNG"),
            Some(bytes.to_vec())
        );

        assert!(fixture.store().list().expect("retry retirement").is_empty());
        assert_eq!(
            fixture
                .store()
                .read_file_bytes(&texture_key)
                .expect("inspect retired PNG"),
            None
        );
    }

    #[tokio::test]
    async fn shutdown_settles_deferred_png_retirement() {
        let root = test_root("shutdown-retirement");
        let store = Arc::new(claim_store(&root).expect("claim skin store"));
        let bytes = b"skin bytes";
        let texture_key = texture_key(bytes);
        store
            .save(
                texture_key.clone(),
                "Saved Skin".to_string(),
                "classic".to_string(),
                "test".to_string(),
                None,
                bytes,
            )
            .expect("save skin");
        store.inject_deferred_file_retirement();
        assert!(matches!(
            store
                .delete_unapplied(&texture_key)
                .expect("commit logical delete"),
            SavedSkinDeleteResult::Deleted(_)
        ));

        store
            .settle_retirements_for_shutdown()
            .await
            .expect("settle deferred retirement for shutdown");

        assert_eq!(
            store
                .read_file_bytes(&texture_key)
                .expect("inspect retired PNG"),
            None
        );
        drop(store);
        fs::remove_dir_all(root).expect("remove test root");
    }

    #[test]
    fn metadata_updates_preserve_or_clear_applied_marker_by_profile_identity() {
        let fixture = SkinStoreFixture::new("metadata-applied-marker");
        let bytes = b"skin bytes";
        let texture_key = texture_key(bytes);
        fixture.save(bytes);
        fixture
            .store()
            .mark_applied(&texture_key)
            .expect("mark applied")
            .expect("skin exists");

        let renamed = fixture
            .store()
            .update_metadata(&texture_key, Some("Renamed".to_string()), None, None)
            .expect("rename skin")
            .expect("skin exists");
        assert!(renamed.applied_at.is_some());

        let changed = fixture
            .store()
            .update_metadata(&texture_key, None, Some("slim".to_string()), None)
            .expect("change applied profile")
            .expect("skin exists");
        assert_eq!(changed.applied_at, None);
    }

    #[test]
    fn same_texture_save_and_replace_track_applied_profile_identity() {
        let fixture = SkinStoreFixture::new("same-texture-applied-marker");
        let bytes = b"skin bytes";
        let texture_key = texture_key(bytes);
        fixture.save(bytes);
        fixture
            .store()
            .mark_applied(&texture_key)
            .expect("mark applied")
            .expect("skin exists");

        let replaced = fixture
            .store()
            .replace_texture(
                &texture_key,
                texture_key.clone(),
                "Renamed".to_string(),
                "classic".to_string(),
                None,
                bytes,
            )
            .expect("replace same texture")
            .expect("skin exists");
        assert!(replaced.applied_at.is_some());

        let resaved = fixture
            .store()
            .save(
                texture_key,
                "Resaved".to_string(),
                "slim".to_string(),
                "test".to_string(),
                None,
                bytes,
            )
            .expect("resave changed profile");
        assert_eq!(resaved.applied_at, None);
    }

    #[test]
    fn replacement_moves_index_removes_old_png_and_clears_applied_marker() {
        let fixture = SkinStoreFixture::new("replace-texture");
        let first = b"first";
        let second = b"second";
        let first_texture_key = texture_key(first);
        let second_texture_key = texture_key(second);
        fixture.save(first);
        fixture
            .store()
            .mark_applied(&first_texture_key)
            .expect("mark applied")
            .expect("skin exists");

        let updated = fixture
            .store()
            .replace_texture(
                &first_texture_key,
                second_texture_key.clone(),
                "Replacement".to_string(),
                "slim".to_string(),
                None,
                second,
            )
            .expect("replace texture")
            .expect("skin exists");

        assert_eq!(updated.texture_key, second_texture_key);
        assert_eq!(updated.applied_at, None);
        assert_eq!(
            fixture
                .store()
                .read_png(&second_texture_key)
                .expect("read replacement")
                .expect("replacement exists"),
            second
        );
        assert_eq!(
            fixture
                .store()
                .read_file_bytes(&first_texture_key)
                .expect("inspect retired PNG"),
            None
        );
    }

    #[test]
    fn texture_identity_and_png_size_are_enforced_at_state_boundary() {
        let fixture = SkinStoreFixture::new("state-bounds");
        let error = fixture
            .store()
            .save(
                "../escape".to_string(),
                "Invalid".to_string(),
                "classic".to_string(),
                "test".to_string(),
                None,
                b"skin",
            )
            .expect_err("unsafe key rejected");
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);

        let oversized = vec![0_u8; SAVED_SKIN_PNG_MAX_BYTES + 1];
        let error = fixture
            .store()
            .save(
                "a".repeat(64),
                "Mismatched".to_string(),
                "classic".to_string(),
                "test".to_string(),
                None,
                b"skin",
            )
            .expect_err("digest mismatch rejected");
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);

        let error = fixture
            .store()
            .save(
                texture_key(&oversized),
                "Oversized".to_string(),
                "classic".to_string(),
                "test".to_string(),
                None,
                &oversized,
            )
            .expect_err("oversized PNG rejected");
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(fixture.store().list().expect("list skins").is_empty());
    }

    #[test]
    fn persisted_skin_metadata_is_bounded() {
        let valid = SavedSkinRecord {
            texture_key: "a".repeat(64),
            name: "Saved Skin".to_string(),
            variant: "classic".to_string(),
            source: "local_upload".to_string(),
            cape_id: None,
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
            applied_at: None,
            byte_size: 1,
        };
        assert!(validate_record(&valid, io::ErrorKind::InvalidData).is_ok());

        for invalid in [
            SavedSkinRecord {
                name: String::new(),
                ..valid.clone()
            },
            SavedSkinRecord {
                variant: "wide".to_string(),
                ..valid.clone()
            },
            SavedSkinRecord {
                source: "invalid source".to_string(),
                ..valid.clone()
            },
            SavedSkinRecord {
                cape_id: Some("invalid cape".to_string()),
                ..valid.clone()
            },
            SavedSkinRecord {
                updated_at: "not-a-timestamp".to_string(),
                ..valid.clone()
            },
            SavedSkinRecord {
                byte_size: 0,
                ..valid
            },
        ] {
            assert_eq!(
                validate_record(&invalid, io::ErrorKind::InvalidData)
                    .expect_err("invalid persisted metadata rejected")
                    .kind(),
                io::ErrorKind::InvalidData
            );
        }
    }

    #[test]
    fn index_aggregate_png_bytes_are_bounded() {
        let record_count =
            (SKIN_INDEX_MAX_TOTAL_PNG_BYTES / SAVED_SKIN_PNG_MAX_BYTES as u64) as usize + 1;
        let index = SavedSkinIndex {
            schema: SKIN_STORE_SCHEMA.to_string(),
            schema_version: SKIN_STORE_SCHEMA_VERSION,
            skins: (0..record_count)
                .map(|value| SavedSkinRecord {
                    texture_key: format!("{value:064x}"),
                    name: "Saved Skin".to_string(),
                    variant: "classic".to_string(),
                    source: "test".to_string(),
                    cape_id: None,
                    created_at: "2026-01-01T00:00:00Z".to_string(),
                    updated_at: "2026-01-01T00:00:00Z".to_string(),
                    applied_at: None,
                    byte_size: SAVED_SKIN_PNG_MAX_BYTES,
                })
                .collect(),
        };

        let error = validate_index(&index).expect_err("aggregate bound enforced");

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    struct SkinStoreFixture {
        root: PathBuf,
        store: Option<SavedSkinStore>,
    }

    impl SkinStoreFixture {
        fn new(name: &str) -> Self {
            let root = test_root(name);
            let paths = AppPaths::from_root(&root).expect("test paths");
            let session = Arc::new(paths.open_root_session().expect("root session"));
            let store = SavedSkinStore::claim(session).expect("claim skin store");
            Self {
                root,
                store: Some(store),
            }
        }

        fn store(&self) -> &SavedSkinStore {
            self.store.as_ref().expect("skin store remains live")
        }

        fn save(&self, bytes: &[u8]) -> SavedSkinRecord {
            self.store()
                .save(
                    texture_key(bytes),
                    "Saved Skin".to_string(),
                    "classic".to_string(),
                    "test".to_string(),
                    None,
                    bytes,
                )
                .expect("save skin")
        }

        fn restart(&mut self) -> io::Result<()> {
            drop(self.store.take());
            self.store = Some(claim_store(&self.root)?);
            Ok(())
        }
    }

    impl Drop for SkinStoreFixture {
        fn drop(&mut self) {
            drop(self.store.take());
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn test_root(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after unix epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("axial-skins-{name}-{}-{nanos}", std::process::id()))
    }

    fn claim_store(root: &Path) -> io::Result<SavedSkinStore> {
        let paths = AppPaths::from_root(root).map_err(io::Error::other)?;
        SavedSkinStore::claim(Arc::new(paths.open_root_session()?))
    }

    fn texture_key(bytes: &[u8]) -> String {
        format!("{:x}", Sha256::digest(bytes))
    }
}
