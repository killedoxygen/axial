//! Saved textures and metadata. Provider/account policy belongs to profile changes.
//!
//! Normalized PNGs are stored with their metadata in one SQLite transaction. No
//! request or persisted record supplies a file path to this feature.

use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::{Deserialize, Deserializer, Serialize};

use crate::library::ApplicationRootPin;
use crate::media::{MediaError, SkinVariant, normalize_skin_png};

use super::store::SavedSkinStore;

pub const LOCAL_UPLOAD_SOURCE: &str = "local_upload";
pub const DEFAULT_SKIN_SOURCE: &str = "minecraft_default_skin";
pub const PROFILE_SKIN_SOURCE: &str = "minecraft_profile_skin";
pub const USERNAME_SKIN_SOURCE: &str = "minecraft_username_skin";

#[derive(Clone, Debug, Serialize)]
pub struct SkinNormalizeResponse {
    pub texture_key: String,
    pub variant_suggestion: SkinVariant,
    pub original_width: u32,
    pub original_height: u32,
    pub normalized_width: u32,
    pub normalized_height: u32,
    pub normalized_byte_size: usize,
    pub normalized_data_url: String,
}

pub fn normalize_upload(bytes: &[u8]) -> Result<SkinNormalizeResponse, SkinLibraryError> {
    let normalized = normalize_skin_png(bytes)?;
    Ok(SkinNormalizeResponse {
        texture_key: crate::media::texture_key(&normalized.png_bytes),
        variant_suggestion: normalized.variant_suggestion,
        original_width: normalized.original_width,
        original_height: normalized.original_height,
        normalized_width: 64,
        normalized_height: 64,
        normalized_byte_size: normalized.png_bytes.len(),
        normalized_data_url: format!(
            "data:image/png;base64,{}",
            STANDARD.encode(&normalized.png_bytes)
        ),
    })
}

/// Existing wardrobe wire shape. Revisions and image bytes remain private.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SavedSkinRecord {
    pub texture_key: String,
    pub name: String,
    pub variant: SkinVariant,
    pub source: String,
    pub cape_id: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub applied_at: Option<String>,
    pub byte_size: usize,
}

/// A monotonic incarnation prevents a late provider completion from marking a
/// changed or deleted-and-recreated library entry as applied.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SavedSkinSnapshot {
    pub record: SavedSkinRecord,
    pub revision: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SavedSkinDeleteResult {
    Deleted(SavedSkinRecord),
    Applied,
    Missing,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SkinLibraryChange {
    Replaced {
        old_texture_key: String,
        new_texture_key: String,
        revision: u64,
    },
    Removed {
        texture_key: String,
    },
}

/// The observer runs synchronously after commit, with library mutation admission
/// retained. It must only update short-lived pending state and must not reenter
/// the library. The lock order is library, then pending-profile state.
pub type SkinLibraryObserver = Arc<dyn Fn(&SkinLibraryChange) + Send + Sync>;

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum SkinLibraryError {
    #[error("Skin name is required and must contain at most 64 supported characters.")]
    InvalidName,
    #[error("Skin variant must be classic or slim.")]
    InvalidVariant,
    #[error("Skin source is not supported.")]
    InvalidSource,
    #[error("Invalid cape id.")]
    InvalidCape,
    #[error("Invalid texture key.")]
    InvalidTextureKey,
    #[error("Invalid account identity.")]
    InvalidAccount,
    #[error("Saved skin data is invalid and could not be loaded safely.")]
    InvalidData,
    #[error("Saved skin library has reached its size limit.")]
    Capacity,
    #[error("Another saved skin update conflicts with this operation. Try again.")]
    Conflict,
    #[error("Saved skin storage is full. Free disk space and try again.")]
    StorageFull,
    #[error("Saved skin storage access was denied. Check app data permissions and try again.")]
    PermissionDenied,
    #[error("Could not update saved skins. Try again.")]
    Storage,
    #[error(transparent)]
    Image(#[from] MediaError),
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub enum CapeUpdate {
    #[default]
    Unchanged,
    Clear,
    Set(String),
}

impl<'de> Deserialize<'de> for CapeUpdate {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Option::<String>::deserialize(deserializer)
            .map(|value| value.map_or(Self::Clear, Self::Set))
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateSavedSkinRequest {
    pub name: Option<String>,
    pub variant: Option<String>,
    #[serde(default)]
    pub cape_id: CapeUpdate,
}

#[derive(Clone, Debug, Default)]
pub struct SaveSkinOptions {
    pub name: String,
    pub variant: Option<SkinVariant>,
    pub cape_id: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct ReplaceSkinOptions {
    pub name: Option<String>,
    pub variant: Option<SkinVariant>,
    pub cape_id: CapeUpdate,
}

#[derive(Clone)]
pub struct SavedSkinLibrary {
    store: SavedSkinStore,
    root: ApplicationRootPin,
    admission: Arc<Mutex<()>>,
    observer: Arc<OnceLock<SkinLibraryObserver>>,
}

impl SavedSkinLibrary {
    pub fn new(store: SavedSkinStore, root: ApplicationRootPin) -> Self {
        Self {
            store,
            root,
            admission: Arc::new(Mutex::new(())),
            observer: Arc::new(OnceLock::new()),
        }
    }

    /// Composition installs the pending-profile observer before exposing routes.
    pub fn set_change_observer(
        &self,
        observer: SkinLibraryObserver,
    ) -> Result<(), SkinLibraryError> {
        let _guard = self.lock()?;
        self.observer
            .set(observer)
            .map_err(|_| SkinLibraryError::Conflict)
    }

    pub fn list(&self) -> Result<Vec<SavedSkinRecord>, SkinLibraryError> {
        let _guard = self.lock()?;
        self.store.list(None)
    }

    /// Applied markers are projected for the selected account without changing
    /// another account's persistent choice.
    pub fn list_for_account(
        &self,
        account_id: &str,
    ) -> Result<Vec<SavedSkinRecord>, SkinLibraryError> {
        validate_account_id(account_id)?;
        let _guard = self.lock()?;
        self.store.list(Some(account_id))
    }

    pub fn get(&self, texture_key: &str) -> Result<Option<SavedSkinSnapshot>, SkinLibraryError> {
        self.with_skin(texture_key, Ok)
    }

    /// Queueing a selection through this callback serializes it with replacement
    /// and removal notifications. The callback must not reenter this library.
    pub fn with_skin<T>(
        &self,
        texture_key: &str,
        read: impl FnOnce(Option<SavedSkinSnapshot>) -> Result<T, SkinLibraryError>,
    ) -> Result<T, SkinLibraryError> {
        let texture_key = validate_texture_key(texture_key)?;
        let _guard = self.lock()?;
        read(self.store.get(&texture_key)?)
    }

    pub fn read_png(&self, texture_key: &str) -> Result<Option<Vec<u8>>, SkinLibraryError> {
        let texture_key = validate_texture_key(texture_key)?;
        let _guard = self.lock()?;
        self.store.read_png(&texture_key)
    }

    pub fn save_upload(
        &self,
        bytes: &[u8],
        options: SaveSkinOptions,
        source: Option<&str>,
    ) -> Result<SavedSkinRecord, SkinLibraryError> {
        let source = validate_upload_source(source)?;
        self.save(bytes, options, source)
    }

    /// The caller has already fenced the account/profile supplying these bytes
    /// and verified ownership of a newly selected cape.
    pub fn save_from_profile(
        &self,
        bytes: &[u8],
        options: SaveSkinOptions,
    ) -> Result<SavedSkinRecord, SkinLibraryError> {
        self.save(bytes, options, PROFILE_SKIN_SOURCE)
    }

    /// Saving an already-current profile and its marker shares library
    /// admission, so an edit cannot change what the marker claims between them.
    pub fn save_current_profile(
        &self,
        account_id: &str,
        bytes: &[u8],
        options: SaveSkinOptions,
    ) -> Result<SavedSkinRecord, SkinLibraryError> {
        validate_account_id(account_id)?;
        let name = validate_name(&options.name)?;
        let cape_id = options
            .cape_id
            .as_deref()
            .map(validate_cape_id)
            .transpose()?;
        let normalized = normalize_skin_png(bytes)?;
        let variant = options.variant.unwrap_or(normalized.variant_suggestion);
        let _guard = self.lock()?;
        let record = self.store.save(
            name,
            variant,
            PROFILE_SKIN_SOURCE,
            cape_id,
            &normalized.png_bytes,
        )?;
        let snapshot = self
            .store
            .get(&record.texture_key)?
            .ok_or(SkinLibraryError::InvalidData)?;
        if !self
            .store
            .mark_applied(account_id, &record.texture_key, Some(snapshot.revision))?
        {
            return Err(SkinLibraryError::Conflict);
        }
        self.notify_updated(&record.texture_key)?;
        Ok(self
            .store
            .get(&record.texture_key)?
            .ok_or(SkinLibraryError::InvalidData)?
            .record)
    }

    pub fn save_from_username(
        &self,
        bytes: &[u8],
        options: SaveSkinOptions,
    ) -> Result<SavedSkinRecord, SkinLibraryError> {
        self.save(bytes, options, USERNAME_SKIN_SOURCE)
    }

    fn save(
        &self,
        bytes: &[u8],
        options: SaveSkinOptions,
        source: &str,
    ) -> Result<SavedSkinRecord, SkinLibraryError> {
        let name = validate_name(&options.name)?;
        let cape_id = options
            .cape_id
            .as_deref()
            .map(validate_cape_id)
            .transpose()?;
        let normalized = normalize_skin_png(bytes)?;
        let variant = options.variant.unwrap_or(normalized.variant_suggestion);
        let _guard = self.lock()?;
        let record = self
            .store
            .save(name, variant, source, cape_id, &normalized.png_bytes)?;
        self.notify_updated(&record.texture_key)?;
        Ok(record)
    }

    pub fn update_metadata(
        &self,
        texture_key: &str,
        update: UpdateSavedSkinRequest,
    ) -> Result<Option<SavedSkinRecord>, SkinLibraryError> {
        let texture_key = validate_texture_key(texture_key)?;
        let name = update.name.as_deref().map(validate_name).transpose()?;
        let variant = update
            .variant
            .as_deref()
            .map(validate_variant)
            .transpose()?;
        let cape = validate_cape_update(&update.cape_id)?;
        let _guard = self.lock()?;
        let record = self
            .store
            .update_metadata(&texture_key, name, variant, cape)?;
        if record.is_some() {
            self.notify_updated(&texture_key)?;
        }
        Ok(record)
    }

    pub fn replace_texture(
        &self,
        texture_key: &str,
        bytes: &[u8],
        options: ReplaceSkinOptions,
    ) -> Result<Option<SavedSkinRecord>, SkinLibraryError> {
        let texture_key = validate_texture_key(texture_key)?;
        let name = options.name.as_deref().map(validate_name).transpose()?;
        let cape = validate_cape_update(&options.cape_id)?;
        let normalized = normalize_skin_png(bytes)?;
        let variant = options.variant.unwrap_or(normalized.variant_suggestion);
        let _guard = self.lock()?;
        let updated =
            self.store
                .replace_texture(&texture_key, name, variant, cape, &normalized.png_bytes)?;
        if let Some(snapshot) = &updated {
            if texture_key != snapshot.record.texture_key {
                // A content-address collision also updates the destination
                // record. Choices already targeting it need the new revision.
                self.notify(&SkinLibraryChange::Replaced {
                    old_texture_key: snapshot.record.texture_key.clone(),
                    new_texture_key: snapshot.record.texture_key.clone(),
                    revision: snapshot.revision,
                });
            }
            self.notify(&SkinLibraryChange::Replaced {
                old_texture_key: texture_key,
                new_texture_key: snapshot.record.texture_key.clone(),
                revision: snapshot.revision,
            });
        }
        Ok(updated.map(|snapshot| snapshot.record))
    }

    pub fn delete_unapplied(
        &self,
        texture_key: &str,
    ) -> Result<SavedSkinDeleteResult, SkinLibraryError> {
        let texture_key = validate_texture_key(texture_key)?;
        let _guard = self.lock()?;
        let result = self.store.delete_unapplied(&texture_key)?;
        if matches!(result, SavedSkinDeleteResult::Deleted(_)) {
            self.notify(&SkinLibraryChange::Removed { texture_key });
        }
        Ok(result)
    }

    /// Provider completions must use this revision-fenced method. `false` means
    /// the provider effect cannot be claimed for the current library entry.
    pub fn mark_applied_if_current(
        &self,
        account_id: &str,
        texture_key: &str,
        revision: u64,
    ) -> Result<bool, SkinLibraryError> {
        validate_account_id(account_id)?;
        let texture_key = validate_texture_key(texture_key)?;
        let _guard = self.lock()?;
        self.store
            .mark_applied(account_id, &texture_key, Some(revision))
    }

    /// For saving an already-current profile after the account owner verifies it.
    pub fn mark_applied(
        &self,
        account_id: &str,
        texture_key: &str,
    ) -> Result<bool, SkinLibraryError> {
        validate_account_id(account_id)?;
        let texture_key = validate_texture_key(texture_key)?;
        let _guard = self.lock()?;
        self.store.mark_applied(account_id, &texture_key, None)
    }

    pub fn clear_applied(&self, account_id: &str) -> Result<(), SkinLibraryError> {
        validate_account_id(account_id)?;
        let _guard = self.lock()?;
        self.store.clear_applied(account_id)
    }

    fn lock(&self) -> Result<MutexGuard<'_, ()>, SkinLibraryError> {
        self.root
            .revalidate()
            .map_err(|_| SkinLibraryError::Storage)?;
        self.admission.lock().map_err(|_| SkinLibraryError::Storage)
    }

    fn notify(&self, change: &SkinLibraryChange) {
        if let Some(observer) = self.observer.get() {
            observer(change);
        }
    }

    fn notify_updated(&self, texture_key: &str) -> Result<(), SkinLibraryError> {
        let snapshot = self
            .store
            .get(texture_key)?
            .ok_or(SkinLibraryError::InvalidData)?;
        self.notify(&SkinLibraryChange::Replaced {
            old_texture_key: texture_key.to_owned(),
            new_texture_key: texture_key.to_owned(),
            revision: snapshot.revision,
        });
        Ok(())
    }
}

pub fn validate_name(value: &str) -> Result<String, SkinLibraryError> {
    let value = value.trim();
    if value.is_empty()
        || value.chars().count() > 64
        || value
            .chars()
            .any(|value| value.is_control() || matches!(value, '/' | '\\'))
    {
        return Err(SkinLibraryError::InvalidName);
    }
    Ok(value.to_owned())
}

pub fn validate_variant(value: &str) -> Result<SkinVariant, SkinLibraryError> {
    match value.trim() {
        "" => Ok(SkinVariant::Classic),
        value if value.eq_ignore_ascii_case("classic") => Ok(SkinVariant::Classic),
        value if value.eq_ignore_ascii_case("slim") => Ok(SkinVariant::Slim),
        _ => Err(SkinLibraryError::InvalidVariant),
    }
}

pub fn validate_upload_source(value: Option<&str>) -> Result<&'static str, SkinLibraryError> {
    match value.map(str::trim).filter(|value| !value.is_empty()) {
        None => Ok(LOCAL_UPLOAD_SOURCE),
        Some(DEFAULT_SKIN_SOURCE) => Ok(DEFAULT_SKIN_SOURCE),
        _ => Err(SkinLibraryError::InvalidSource),
    }
}

pub fn validate_cape_id(value: &str) -> Result<String, SkinLibraryError> {
    let value = value.trim();
    if value.is_empty() || value.len() > 80 || !portable_token(value) {
        return Err(SkinLibraryError::InvalidCape);
    }
    Ok(value.to_owned())
}

pub fn validate_texture_key(value: &str) -> Result<String, SkinLibraryError> {
    let value = value.trim();
    if value.len() != 64
        || !value
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    {
        return Err(SkinLibraryError::InvalidTextureKey);
    }
    Ok(value.to_owned())
}

pub fn default_profile_skin_name(profile_name: &str) -> String {
    format!("{} profile skin", profile_name.trim())
        .chars()
        .take(64)
        .collect()
}

pub fn default_username_skin_name(profile_name: &str) -> String {
    format!("{} skin", profile_name.trim())
        .chars()
        .take(64)
        .collect()
}

fn validate_cape_update(update: &CapeUpdate) -> Result<Option<Option<String>>, SkinLibraryError> {
    match update {
        CapeUpdate::Unchanged => Ok(None),
        CapeUpdate::Clear => Ok(Some(None)),
        CapeUpdate::Set(id) => Ok(Some(Some(validate_cape_id(id)?))),
    }
}

pub(super) fn validate_account_id(value: &str) -> Result<(), SkinLibraryError> {
    if value.is_empty() || value.len() > 128 || !portable_token(value) {
        return Err(SkinLibraryError::InvalidAccount);
    }
    Ok(())
}

pub(super) fn portable_token(value: &str) -> bool {
    value
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cape_json_distinguishes_omitted_null_and_selection() {
        let omitted: UpdateSavedSkinRequest = serde_json::from_str("{}").unwrap();
        let clear: UpdateSavedSkinRequest = serde_json::from_str(r#"{"cape_id":null}"#).unwrap();
        let set: UpdateSavedSkinRequest =
            serde_json::from_str(r#"{"cape_id":"cape-one"}"#).unwrap();
        assert_eq!(omitted.cape_id, CapeUpdate::Unchanged);
        assert_eq!(clear.cape_id, CapeUpdate::Clear);
        assert_eq!(set.cape_id, CapeUpdate::Set("cape-one".into()));
    }

    #[test]
    fn metadata_validation_matches_retained_boundaries() {
        assert_eq!(validate_name("  My skin  ").unwrap(), "My skin");
        assert!(validate_name(&"é".repeat(64)).is_ok());
        for bad in ["", "   ", "../skin", "skin\\file", "skin\nfile"] {
            assert_eq!(validate_name(bad), Err(SkinLibraryError::InvalidName));
        }
        assert!(validate_name(&"a".repeat(65)).is_err());
        assert_eq!(validate_variant(" SLIM ").unwrap(), SkinVariant::Slim);
        assert_eq!(validate_variant("").unwrap(), SkinVariant::Classic);
        assert!(validate_upload_source(Some(PROFILE_SKIN_SOURCE)).is_err());
        assert_eq!(
            validate_upload_source(Some(DEFAULT_SKIN_SOURCE)).unwrap(),
            DEFAULT_SKIN_SOURCE
        );
        assert!(validate_cape_id("cape/secret").is_err());
        assert!(validate_texture_key(&"A".repeat(64)).is_err());
    }

    #[test]
    fn replacing_into_an_existing_texture_preserves_both_accounts_pending_choices() {
        use crate::{
            accounts::directory::AccountDirectory,
            library::{LibraryLifecycle, LibraryOpenOutcome},
            skins::{pending::PendingSkins, store::MIGRATION, tests::png},
            storage::MetadataStore,
        };
        let directory = tempfile::tempdir().unwrap();
        let roots = match LibraryLifecycle::open(&directory.path().canonicalize().unwrap()) {
            LibraryOpenOutcome::Ready(roots) => roots,
            _ => panic!("isolated root"),
        };
        let metadata = Arc::new(MetadataStore::in_memory().unwrap());
        let accounts = AccountDirectory::new(metadata.clone()).unwrap();
        accounts.create_offline_account("Alice").unwrap();
        let alice = accounts.capture_selected().unwrap();
        accounts.create_offline_account("Bob").unwrap();
        let bob = accounts.capture_selected().unwrap();
        metadata.migrate(&[MIGRATION]).unwrap();
        let library = SavedSkinLibrary::new(
            SavedSkinStore::new(metadata),
            roots.admit_application_root().unwrap(),
        );
        let pending = Arc::new(PendingSkins::default());
        let observer = pending.clone();
        library
            .set_change_observer(Arc::new(move |change| observer.library_changed(change)))
            .unwrap();
        let first = library
            .save_upload(
                &png(11),
                SaveSkinOptions {
                    name: "First".into(),
                    ..Default::default()
                },
                None,
            )
            .unwrap();
        let second = library
            .save_upload(
                &png(22),
                SaveSkinOptions {
                    name: "Second".into(),
                    ..Default::default()
                },
                None,
            )
            .unwrap();
        pending.queue(
            &alice,
            first.texture_key.clone(),
            library.get(&first.texture_key).unwrap().unwrap().revision,
        );
        pending.queue(
            &bob,
            second.texture_key.clone(),
            library.get(&second.texture_key).unwrap().unwrap().revision,
        );
        library
            .replace_texture(&first.texture_key, &png(22), ReplaceSkinOptions::default())
            .unwrap();
        let updated = library.get(&second.texture_key).unwrap().unwrap();
        for capture in [&alice, &bob] {
            let intent = pending.claim(capture.account_id(), None).unwrap().unwrap();
            assert_eq!(intent.texture_key, updated.record.texture_key);
            assert_eq!(intent.skin_revision, updated.revision);
        }
    }

    #[cfg(unix)]
    #[test]
    fn upload_revalidates_actual_library_root_before_any_database_write() {
        use crate::{
            accounts::directory::AccountDirectory,
            library::{LibraryLifecycle, LibraryOpenOutcome},
            skins::store::MIGRATION,
            storage::MetadataStore,
        };
        let parent = tempfile::tempdir().unwrap();
        let parent = parent.path().canonicalize().unwrap();
        let path = parent.join("profile");
        std::fs::create_dir(&path).unwrap();
        let roots = match LibraryLifecycle::open(&path) {
            LibraryOpenOutcome::Ready(roots) => roots,
            _ => panic!("isolated root"),
        };
        let metadata = Arc::new(MetadataStore::in_memory().unwrap());
        AccountDirectory::new(metadata.clone()).unwrap();
        metadata.migrate(&[MIGRATION]).unwrap();
        let library = SavedSkinLibrary::new(
            SavedSkinStore::new(metadata),
            roots.admit_application_root().unwrap(),
        );
        let retained = roots.admit_application_root().unwrap();
        retained.revalidate().unwrap();
        std::fs::rename(&path, parent.join("original-profile")).unwrap();
        std::fs::create_dir(&path).unwrap();
        std::fs::write(path.join("canary"), b"unrelated replacement").unwrap();
        assert!(retained.revalidate().is_err());
        assert_eq!(
            library.save_upload(
                &crate::skins::tests::png(11),
                SaveSkinOptions {
                    name: "Retained skin".into(),
                    ..Default::default()
                },
                None,
            ),
            Err(SkinLibraryError::Storage)
        );
        assert!(library.store.list(None).unwrap().is_empty());
        assert_eq!(
            std::fs::read(path.join("canary")).unwrap(),
            b"unrelated replacement"
        );
    }
}
