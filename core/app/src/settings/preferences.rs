//! Profile-scoped interface preferences. Their revision is independent of the
//! launch/configuration fence, and their payload never enters ConfigView's watch.

use std::{collections::BTreeMap, io};

use serde::{Deserialize, Deserializer, Serialize};
use ts_rs::TS;

use super::{MAX_REVISION, SettingsError, SettingsStore, encode, next_revision, required_document};

/// A predecessor export is at most 1 MiB. A separate 1 MiB allowance covers
/// normalization and reference expansion; the maximum-export fixture measures
/// those effects. This encoded UTF-8 cap is not a UTF-16 string-length limit.
pub const MAX_INTERFACE_PREFERENCES_BYTES: usize = 2 * 1024 * 1024;
const MAX_ENTRIES: usize = 4096;

fn invalid() -> SettingsError {
    SettingsError::Validation("Interface preferences contain unsupported or invalid values.")
}

fn present<'de, D: Deserializer<'de>, T: Deserialize<'de>>(de: D) -> Result<Option<T>, D::Error> {
    T::deserialize(de).map(Some)
}

fn nullable<'de, D: Deserializer<'de>, T: Deserialize<'de>>(de: D) -> Result<Option<T>, D::Error> {
    Option::<T>::deserialize(de)
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct ShortcutBinding {
    pub key: String,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    #[ts(optional)]
    pub ctrl: Option<bool>,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    #[ts(optional)]
    pub shift: Option<bool>,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    #[ts(optional)]
    pub alt: Option<bool>,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    #[ts(optional)]
    pub meta: Option<bool>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OverlayPosition {
    pub x: f64,
    pub y: f64,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    #[ts(optional)]
    pub scale_x: Option<f64>,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    #[ts(optional)]
    pub scale_y: Option<f64>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize, TS)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct LocalPreferences {
    pub theme: String,
    pub custom_hue: f64,
    pub custom_vibrancy: f64,
    pub lightness: f64,
    pub sounds: bool,
    pub hide_skin_nametag: bool,
    pub selected_skin: String,
    pub selected_skins_by_account: BTreeMap<String, String>,
    pub shortcuts: BTreeMap<String, ShortcutBinding>,
    pub overlay_positions: BTreeMap<String, OverlayPosition>,
    pub last_update_check_at: String,
    pub dismissed_update_version: String,
}

impl Default for LocalPreferences {
    fn default() -> Self {
        Self {
            theme: "obsidian".into(),
            custom_hue: 140.0,
            custom_vibrancy: 100.0,
            lightness: 0.0,
            sounds: true,
            hide_skin_nametag: false,
            selected_skin: String::new(),
            selected_skins_by_account: BTreeMap::new(),
            shortcuts: BTreeMap::new(),
            overlay_positions: BTreeMap::new(),
            last_update_check_at: String::new(),
            dismissed_update_version: String::new(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize, TS)]
#[serde(tag = "name", rename_all = "kebab-case", deny_unknown_fields)]
pub enum InterfaceRoute {
    Home {},
    Instances {},
    Instance {
        id: String,
    },
    Discover {
        #[serde(
            default,
            deserialize_with = "present",
            skip_serializing_if = "Option::is_none"
        )]
        #[ts(optional)]
        target: Option<String>,
    },
    Content {
        id: String,
        #[serde(
            default,
            deserialize_with = "present",
            skip_serializing_if = "Option::is_none"
        )]
        #[ts(optional)]
        target: Option<String>,
    },
    DevLab {},
    Downloads {},
    Accounts {},
    Settings {},
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct InterfacePreferences {
    pub version: u8,
    pub preferences: LocalPreferences,
    #[serde(deserialize_with = "nullable")]
    pub route: Option<InterfaceRoute>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct InterfacePreferencesSnapshot {
    pub revision: u64,
    #[serde(deserialize_with = "nullable")]
    pub value: Option<InterfacePreferences>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct InterfacePreferencesUpdate {
    pub expected_revision: u64,
    pub change: InterfacePreferencesChange,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize, TS)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum InterfacePreferencesChange {
    Local {
        preferences: LocalPreferences,
    },
    Route {
        #[serde(deserialize_with = "nullable")]
        route: Option<InterfaceRoute>,
    },
    Replace {
        #[serde(deserialize_with = "nullable")]
        value: Option<InterfacePreferences>,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct InterfacePreferencesReceipt {
    pub revision: u64,
}

impl SettingsStore {
    pub fn interface_preferences(&self) -> Result<InterfacePreferencesSnapshot, SettingsError> {
        self.metadata.read(|connection| {
            let document = required_document(connection)?;
            Ok(InterfacePreferencesSnapshot {
                revision: document.interface_preferences_revision,
                value: document.interface_preferences,
            })
        })
    }

    /// No ConfigView publication: changing a route cannot invalidate an already
    /// prepared launch. Read both owners' latest fields under the metadata lock.
    pub fn update_interface_preferences(
        &self,
        update: InterfacePreferencesUpdate,
    ) -> Result<InterfacePreferencesReceipt, SettingsError> {
        if update.expected_revision > MAX_REVISION {
            return Err(invalid());
        }
        match &update.change {
            InterfacePreferencesChange::Local { preferences } => {
                preferences.validate()?;
                bounded_size(preferences)?;
            }
            InterfacePreferencesChange::Route { route } => validate_route(route.as_ref())?,
            InterfacePreferencesChange::Replace { value } => {
                if let Some(value) = value {
                    value.validate()?;
                }
            }
        }
        self.metadata.transaction(|transaction| {
            let mut document = required_document(transaction)?;
            if document.interface_preferences_revision != update.expected_revision {
                return Err(SettingsError::Conflict);
            }
            match update.change {
                InterfacePreferencesChange::Local { preferences } => {
                    document
                        .interface_preferences
                        .as_mut()
                        .ok_or_else(invalid)?
                        .preferences = preferences;
                }
                InterfacePreferencesChange::Route { route } => {
                    document
                        .interface_preferences
                        .as_mut()
                        .ok_or_else(invalid)?
                        .route = route;
                }
                InterfacePreferencesChange::Replace { value } => {
                    document.interface_preferences = value
                }
            }
            if let Some(value) = &document.interface_preferences {
                value.validate()?;
            }
            document.interface_preferences_revision = next_revision(update.expected_revision)?;
            let changed = transaction.execute(
                "UPDATE settings_config SET document=?1 WHERE singleton=1",
                [encode(&document)?],
            )?;
            if changed != 1 {
                return Err(SettingsError::Conflict);
            }
            Ok(InterfacePreferencesReceipt {
                revision: document.interface_preferences_revision,
            })
        })
    }
}

impl InterfacePreferences {
    pub(super) fn validate(&self) -> Result<(), SettingsError> {
        if self.version != 1 {
            return Err(invalid());
        }
        self.preferences.validate()?;
        validate_route(self.route.as_ref())?;
        bounded_size(self)
    }
}

impl LocalPreferences {
    fn validate(&self) -> Result<(), SettingsError> {
        if !matches!(
            self.theme.as_str(),
            "obsidian" | "deepslate" | "nether" | "end" | "birch" | "custom"
        ) {
            return Err(invalid());
        }
        number(self.custom_hue, 0.0, 360.0)?;
        number(self.custom_vibrancy, 0.0, 100.0)?;
        number(self.lightness, 0.0, 100.0)?;
        for value in [
            &self.selected_skin,
            &self.last_update_check_at,
            &self.dismissed_update_version,
        ] {
            text(value, 1024)?;
        }
        keys(&self.selected_skins_by_account)?;
        for value in self.selected_skins_by_account.values() {
            text(value, 1024)?;
        }
        keys(&self.shortcuts)?;
        for value in self.shortcuts.values() {
            text(&value.key, 64)?;
            if value.key.is_empty() {
                return Err(invalid());
            }
        }
        keys(&self.overlay_positions)?;
        for value in self.overlay_positions.values() {
            number(value.x, -1_000_000.0, 1_000_000.0)?;
            number(value.y, -1_000_000.0, 1_000_000.0)?;
            for scale in [value.scale_x, value.scale_y].into_iter().flatten() {
                number(scale, 0.0, 1.0)?;
            }
        }
        Ok(())
    }
}

fn validate_route(route: Option<&InterfaceRoute>) -> Result<(), SettingsError> {
    if let Some(InterfaceRoute::Instance { id } | InterfaceRoute::Content { id, .. }) = route {
        text(id, 1024)?;
        if id.is_empty() {
            return Err(invalid());
        }
    }
    if let Some(InterfaceRoute::Discover { target } | InterfaceRoute::Content { target, .. }) =
        route
    {
        if let Some(target) = target {
            text(target, 1024)?;
        }
    }
    Ok(())
}

fn text(value: &str, limit: usize) -> Result<(), SettingsError> {
    if value.encode_utf16().count() > limit || value.chars().any(|c| c <= '\u{1f}') {
        return Err(invalid());
    }
    Ok(())
}

fn keys<T>(values: &BTreeMap<String, T>) -> Result<(), SettingsError> {
    if values.len() > MAX_ENTRIES
        || values.keys().any(|key| {
            key.encode_utf16().count() > 1024
                || matches!(key.as_str(), "__proto__" | "prototype" | "constructor")
        })
    {
        return Err(invalid());
    }
    Ok(())
}

fn number(value: f64, min: f64, max: f64) -> Result<(), SettingsError> {
    if !value.is_finite() || !(min..=max).contains(&value) {
        return Err(invalid());
    }
    Ok(())
}

/// Count encoded bytes without allocating another potentially large payload.
fn bounded_size(value: &impl Serialize) -> Result<(), SettingsError> {
    struct Limit(usize);
    impl io::Write for Limit {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0 = self
                .0
                .checked_sub(bytes.len())
                .ok_or_else(|| io::Error::other("preference size limit"))?;
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    serde_json::to_writer(Limit(MAX_INTERFACE_PREFERENCES_BYTES), value).map_err(|_| invalid())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        settings::{ConfigPatch, ConfigTheme, ConfigView, prepare_legacy_import},
        storage::{MetadataStore, StorageError},
    };
    use serde_json::{Value, json};
    use std::sync::{Arc, Barrier};

    fn store() -> (Arc<MetadataStore>, Arc<SettingsStore>) {
        let metadata = Arc::new(MetadataStore::in_memory().unwrap());
        let settings = Arc::new(SettingsStore::new(metadata.clone()).unwrap());
        (metadata, settings)
    }

    fn envelope() -> InterfacePreferences {
        serde_json::from_value(json!({
            "version":1,
            "preferences":{
                "theme":"custom", "customHue":215.125, "customVibrancy":73.25, "lightness":4.5,
                "sounds":false, "hideSkinNametag":true, "selectedSkin":"default:steve",
                "selectedSkinsByAccount":{"account:fallback":"default:alex"},
                "shortcuts":{"launch":{"key":"K","ctrl":true,"shift":false}},
                "overlayPositions":{"music":{"x":-12.5,"y":16.25,"scaleX":0.125,"scaleY":1.0}},
                "lastUpdateCheckAt":"2026-09-27T00:00:00Z", "dismissedUpdateVersion":"1.2.3"
            },
            "route":{"name":"content","id":"project","target":"instance"}
        }))
        .unwrap()
    }

    fn replace(
        settings: &SettingsStore,
        revision: u64,
        value: Option<InterfacePreferences>,
    ) -> Result<InterfacePreferencesReceipt, SettingsError> {
        settings.update_interface_preferences(InterfacePreferencesUpdate {
            expected_revision: revision,
            change: InterfacePreferencesChange::Replace { value },
        })
    }

    fn raw(metadata: &MetadataStore) -> String {
        metadata
            .read(|db| -> Result<_, StorageError> {
                Ok(db.query_row(
                    "SELECT document FROM settings_config WHERE singleton=1",
                    [],
                    |row| row.get(0),
                )?)
            })
            .unwrap()
    }

    #[test]
    fn old_document_and_disk_reopen_preserve_independent_preferences() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory
            .path()
            .canonicalize()
            .unwrap()
            .join("settings.sqlite");
        let expected = envelope();
        {
            let metadata = Arc::new(MetadataStore::open(&database).unwrap());
            let settings = SettingsStore::new(metadata.clone()).unwrap();
            let mut original: Value = serde_json::from_str(&raw(&metadata)).unwrap();
            original
                .as_object_mut()
                .unwrap()
                .remove("interface_preferences_revision");
            original
                .as_object_mut()
                .unwrap()
                .remove("interface_preferences");
            metadata
                .transaction(|tx| -> Result<(), StorageError> {
                    tx.execute(
                        "UPDATE settings_config SET document=?1",
                        [original.to_string()],
                    )?;
                    Ok(())
                })
                .unwrap();
            let before = raw(&metadata);
            assert_eq!(
                settings.interface_preferences().unwrap(),
                InterfacePreferencesSnapshot {
                    revision: 0,
                    value: None
                }
            );
            assert_eq!(raw(&metadata), before);
            assert_eq!(
                replace(&settings, 0, Some(expected.clone()))
                    .unwrap()
                    .revision,
                1
            );
            assert_eq!(settings.current().unwrap(), ConfigView::default());
        }
        let metadata = Arc::new(MetadataStore::open(&database).unwrap());
        let settings = SettingsStore::new(metadata).unwrap();
        assert_eq!(
            settings.interface_preferences().unwrap(),
            InterfacePreferencesSnapshot {
                revision: 1,
                value: Some(expected)
            }
        );
        assert_eq!(settings.current().unwrap(), ConfigView::default());
    }

    #[test]
    fn partials_require_initialization_and_clear_does_not_reset_the_revision() {
        let (metadata, settings) = store();
        for change in [
            InterfacePreferencesChange::Local {
                preferences: LocalPreferences::default(),
            },
            InterfacePreferencesChange::Route { route: None },
        ] {
            let before = raw(&metadata);
            assert!(matches!(
                settings.update_interface_preferences(InterfacePreferencesUpdate {
                    expected_revision: 0,
                    change
                }),
                Err(SettingsError::Validation(_))
            ));
            assert_eq!(raw(&metadata), before);
        }
        let initial = envelope();
        replace(&settings, 0, Some(initial.clone())).unwrap();
        settings
            .update_interface_preferences(InterfacePreferencesUpdate {
                expected_revision: 1,
                change: InterfacePreferencesChange::Route { route: None },
            })
            .unwrap();
        let mut expected = initial;
        expected.route = None;
        assert_eq!(
            settings.interface_preferences().unwrap().value,
            Some(expected.clone())
        );
        expected.preferences.sounds = true;
        settings
            .update_interface_preferences(InterfacePreferencesUpdate {
                expected_revision: 2,
                change: InterfacePreferencesChange::Local {
                    preferences: expected.preferences.clone(),
                },
            })
            .unwrap();
        assert_eq!(
            settings.interface_preferences().unwrap().value,
            Some(expected)
        );
        replace(&settings, 3, None).unwrap();
        assert_eq!(
            settings.interface_preferences().unwrap(),
            InterfacePreferencesSnapshot {
                revision: 4,
                value: None
            }
        );
        assert!(matches!(
            replace(&settings, 0, Some(envelope())),
            Err(SettingsError::Conflict)
        ));
    }

    #[test]
    fn ui_writes_preserve_config_fences_notifications_and_metadata_import() {
        let (_, settings) = store();
        let mut changes = settings.subscribe().unwrap();
        let global = settings.current().unwrap();
        let launch = crate::settings::InstanceSettings::default()
            .effective(&global)
            .unwrap();
        replace(&settings, 0, Some(envelope())).unwrap();
        for revision in 1..4 {
            settings
                .update_interface_preferences(InterfacePreferencesUpdate {
                    expected_revision: revision,
                    change: InterfacePreferencesChange::Route {
                        route: Some(InterfaceRoute::Settings {}),
                    },
                })
                .unwrap();
        }
        settings
            .validate_revision(launch.global_config_revision)
            .unwrap();
        assert_eq!(settings.current().unwrap(), global);
        assert!(!changes.has_changed().unwrap());
        let snapshot = settings.interface_preferences().unwrap();
        let patch: ConfigPatch =
            serde_json::from_value(json!({"expected_revision":0,"theme":"birch"})).unwrap();
        settings.update(patch).unwrap();
        assert!(changes.has_changed().unwrap());
        changes.borrow_and_update();
        let imported = prepare_legacy_import(
            &json!({"username":"Imported","min_memory_mb":512,"max_memory_mb":6144}),
        )
        .unwrap();
        settings
            .commit_prepared_import(
                &imported,
                1,
                |_, config| {
                    config.account_selection_revision = 7;
                    Ok((true, ()))
                },
                |_, _| Ok(()),
            )
            .unwrap();
        assert_eq!(settings.interface_preferences().unwrap(), snapshot);
        assert_eq!(settings.current().unwrap().revision, 2);
        assert_eq!(settings.current().unwrap().account_selection_revision, 7);
        assert!(changes.has_changed().unwrap());
    }

    #[test]
    fn simultaneous_ui_and_config_updates_preserve_both_domains() {
        let (_, settings) = store();
        replace(&settings, 0, Some(envelope())).unwrap();
        let barrier = Arc::new(Barrier::new(3));
        let ui = {
            let settings = settings.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                settings.update_interface_preferences(InterfacePreferencesUpdate {
                    expected_revision: 1,
                    change: InterfacePreferencesChange::Route {
                        route: Some(InterfaceRoute::Home {}),
                    },
                })
            })
        };
        let config = {
            let settings = settings.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                settings.update(
                    serde_json::from_value(json!({"expected_revision":0,"theme":"birch"})).unwrap(),
                )
            })
        };
        barrier.wait();
        ui.join().unwrap().unwrap();
        config.join().unwrap().unwrap();
        assert_eq!(settings.current().unwrap().theme, ConfigTheme::Birch);
        assert_eq!(settings.current().unwrap().revision, 1);
        let preferences = settings.interface_preferences().unwrap();
        assert_eq!(preferences.revision, 2);
        assert_eq!(
            preferences.value.unwrap().route,
            Some(InterfaceRoute::Home {})
        );
    }

    #[test]
    fn failed_or_stale_ui_writes_never_publish_a_receipt_or_change_bytes() {
        let (metadata, settings) = store();
        let changes = settings.subscribe().unwrap();
        replace(&settings, 0, Some(envelope())).unwrap();
        let before = raw(&metadata);
        assert!(matches!(
            replace(&settings, 0, None),
            Err(SettingsError::Conflict)
        ));
        metadata.transaction(|tx| -> Result<(), StorageError> {
            tx.execute_batch("CREATE TRIGGER fail_preferences BEFORE UPDATE ON settings_config BEGIN SELECT RAISE(ABORT,'fixture'); END")?;
            Ok(())
        }).unwrap();
        assert!(matches!(
            replace(&settings, 1, None),
            Err(SettingsError::Storage(_))
        ));
        assert_eq!(raw(&metadata), before);
        assert_eq!(settings.interface_preferences().unwrap().revision, 1);
        assert_eq!(settings.current().unwrap().revision, 0);
        assert!(!changes.has_changed().unwrap());
        metadata.transaction(|tx| -> Result<(), StorageError> {
            tx.execute_batch("DROP TRIGGER fail_preferences;
                CREATE TRIGGER fail_preferences BEFORE UPDATE ON settings_config BEGIN SELECT RAISE(IGNORE); END")?;
            Ok(())
        }).unwrap();
        assert!(matches!(
            replace(&settings, 1, None),
            Err(SettingsError::Conflict)
        ));
        assert_eq!(raw(&metadata), before);
        assert_eq!(settings.interface_preferences().unwrap().revision, 1);
        assert_eq!(settings.current().unwrap().revision, 0);
        assert!(!changes.has_changed().unwrap());
    }

    #[test]
    fn wire_shapes_and_validation_match_current_local_preferences() {
        let value = envelope();
        value.validate().unwrap();
        let encoded = serde_json::to_value(&value).unwrap();
        assert_eq!(encoded["preferences"].as_object().unwrap().len(), 12);
        assert_eq!(encoded["preferences"]["customHue"], json!(215.125));
        assert_eq!(
            encoded["preferences"]["overlayPositions"]["music"]["scaleX"],
            json!(0.125)
        );
        assert_eq!(
            serde_json::from_value::<LocalPreferences>(json!({})).unwrap(),
            LocalPreferences::default()
        );
        for route in [
            json!({"name":"home"}),
            json!({"name":"instances"}),
            json!({"name":"instance","id":"x"}),
            json!({"name":"discover"}),
            json!({"name":"discover","target":""}),
            json!({"name":"content","id":"x"}),
            json!({"name":"content","id":"x","target":"y"}),
            json!({"name":"dev-lab"}),
            json!({"name":"downloads"}),
            json!({"name":"accounts"}),
            json!({"name":"settings"}),
        ] {
            let decoded: InterfaceRoute = serde_json::from_value(route.clone()).unwrap();
            validate_route(Some(&decoded)).unwrap();
            assert_eq!(serde_json::to_value(decoded).unwrap(), route);
        }
        for malformed in [
            json!({"sounds":null}),
            json!({"unknown":false}),
            json!({"shortcuts":{"launch":{"key":"K","ctrl":null}}}),
            json!({"overlayPositions":{"music":{"x":1,"y":2,"scaleX":null}}}),
        ] {
            assert!(serde_json::from_value::<LocalPreferences>(malformed).is_err());
        }
        // Normal UI identifiers come from valid-Unicode owners. An export with
        // a lone UTF-16 surrogate is malformed input, not a lossy replacement.
        assert!(serde_json::from_str::<LocalPreferences>(r#"{"selectedSkin":"\ud800"}"#).is_err());
        for malformed in [
            json!({"name":"home","id":"x"}),
            json!({"name":"discover","target":null}),
            json!({"name":"guardian"}),
        ] {
            assert!(serde_json::from_value::<InterfaceRoute>(malformed).is_err());
        }
        for malformed in [
            json!({"kind":"route"}),
            json!({"kind":"replace"}),
            json!({"kind":"route","route":null,"extra":1}),
        ] {
            assert!(serde_json::from_value::<InterfacePreferencesChange>(malformed).is_err());
        }
        for field in ["customHue", "customVibrancy", "lightness"] {
            let mut invalid = encoded.clone();
            invalid["preferences"][field] = json!(-0.5);
            assert!(
                serde_json::from_value::<InterfacePreferences>(invalid)
                    .unwrap()
                    .validate()
                    .is_err()
            );
        }
        for invalid in [
            json!({"theme":"unknown"}),
            json!({"selectedSkin":"line\nbreak"}),
            json!({"shortcuts":{"launch":{"key":""}}}),
            json!({"overlayPositions":{"music":{"x":1000000.5,"y":0}}}),
            json!({"overlayPositions":{"music":{"x":0,"y":0,"scaleY":1.01}}}),
            json!({"selectedSkinsByAccount":{"__proto__":"x"}}),
        ] {
            assert!(
                serde_json::from_value::<LocalPreferences>(invalid)
                    .unwrap()
                    .validate()
                    .is_err()
            );
        }
        let mut preferences = LocalPreferences::default();
        preferences.selected_skin = "💾".repeat(512);
        preferences.validate().unwrap();
        preferences.selected_skin.push('x');
        assert!(preferences.validate().is_err());
        preferences.selected_skin = "界".repeat(1024);
        preferences.validate().unwrap();
        preferences = LocalPreferences::default();
        preferences.custom_hue = f64::NAN;
        assert!(preferences.validate().is_err());
    }

    #[test]
    fn invalid_stored_preferences_and_revision_exhaustion_preserve_existing_bytes() {
        let (metadata, settings) = store();
        replace(&settings, 0, Some(envelope())).unwrap();
        let original: Value = serde_json::from_str(&raw(&metadata)).unwrap();
        for invalid in [
            json!({"version":2,"preferences":{},"route":null}),
            json!({"version":1,"preferences":{"lightness":101},"route":null}),
        ] {
            let mut document = original.clone();
            document["interface_preferences"] = invalid;
            metadata
                .transaction(|tx| -> Result<(), StorageError> {
                    tx.execute(
                        "UPDATE settings_config SET document=?1",
                        [document.to_string()],
                    )?;
                    Ok(())
                })
                .unwrap();
            let before = raw(&metadata);
            assert!(matches!(
                settings.interface_preferences(),
                Err(SettingsError::Corrupt)
            ));
            assert!(matches!(
                replace(&settings, 1, None),
                Err(SettingsError::Corrupt)
            ));
            assert_eq!(raw(&metadata), before);
        }
        let mut exhausted = original;
        exhausted["interface_preferences_revision"] = json!(MAX_REVISION);
        metadata
            .transaction(|tx| -> Result<(), StorageError> {
                tx.execute(
                    "UPDATE settings_config SET document=?1",
                    [exhausted.to_string()],
                )?;
                Ok(())
            })
            .unwrap();
        let before = raw(&metadata);
        assert!(matches!(
            replace(&settings, MAX_REVISION, None),
            Err(SettingsError::Unavailable)
        ));
        assert_eq!(raw(&metadata), before);
    }

    fn expanded_export() -> (usize, InterfacePreferences) {
        let mut preferences = LocalPreferences::default();
        for index in 0..MAX_ENTRIES {
            preferences
                .selected_skins_by_account
                .insert(format!("account:{index:016x}"), String::new());
            preferences.shortcuts.insert(
                format!("🔉{index}"),
                ShortcutBinding {
                    key: "K".into(),
                    ctrl: None,
                    shift: None,
                    alt: None,
                    meta: None,
                },
            );
            preferences.overlay_positions.insert(
                index.to_string(),
                OverlayPosition {
                    x: 1e-6,
                    y: -1e-6,
                    scale_x: Some(0.00001),
                    scale_y: Some(0.99999),
                },
            );
        }
        let export = |preferences: &LocalPreferences| {
            json!({
                "format":"axial-browser-preferences","version":1,"preferences":{
                    "selectedSkinsByAccount":preferences.selected_skins_by_account,
                    "shortcuts":preferences.shortcuts,
                    "overlayPositions":preferences.overlay_positions
                },
                "route":{"name":"instance","id":"0000000000000001"}
            })
        };
        let limit = 1024 * 1024;
        let remaining = limit - serde_json::to_vec(&export(&preferences)).unwrap().len();
        for (index, value) in preferences
            .selected_skins_by_account
            .values_mut()
            .enumerate()
        {
            *value =
                "x".repeat(remaining / MAX_ENTRIES + usize::from(index < remaining % MAX_ENTRIES));
        }
        let source = export(&preferences);
        let source_bytes = serde_json::to_vec(&source).unwrap().len();
        assert_eq!(source_bytes, limit);
        preferences = serde_json::from_value(source["preferences"].clone()).unwrap();
        preferences.selected_skins_by_account = preferences
            .selected_skins_by_account
            .into_values()
            .enumerate()
            .map(|(index, value)| {
                (
                    format!("account:microsoft-{}", uuid::Uuid::from_u128(index as u128)),
                    value,
                )
            })
            .collect();
        (
            source_bytes,
            InterfacePreferences {
                version: 1,
                preferences,
                route: Some(InterfaceRoute::Instance {
                    id: "12345678-1234-4234-8234-123456789abc".into(),
                }),
            },
        )
    }

    #[test]
    fn maximum_export_and_reference_expansion_fit_without_narrowing_to_config_limit() {
        let (source_bytes, value) = expanded_export();
        let normalized_bytes = serde_json::to_vec(&value).unwrap().len();
        assert!(normalized_bytes > source_bytes);
        assert!(normalized_bytes <= MAX_INTERFACE_PREFERENCES_BYTES);
        value.validate().unwrap();
        let (_, settings) = store();
        replace(&settings, 0, Some(value.clone())).unwrap();
        assert_eq!(
            settings.interface_preferences().unwrap().value,
            Some(value.clone())
        );
        let mut excessive_entries = value.clone();
        excessive_entries.preferences.shortcuts.insert(
            "one-too-many".into(),
            ShortcutBinding {
                key: "K".into(),
                ctrl: None,
                shift: None,
                alt: None,
                meta: None,
            },
        );
        assert!(replace(&settings, 1, Some(excessive_entries)).is_err());
        let mut excessive_bytes = value;
        for selected in excessive_bytes
            .preferences
            .selected_skins_by_account
            .values_mut()
        {
            *selected = "界".repeat(200);
        }
        assert!(
            serde_json::to_vec(&excessive_bytes).unwrap().len() > MAX_INTERFACE_PREFERENCES_BYTES
        );
        assert!(replace(&settings, 1, Some(excessive_bytes)).is_err());
        assert_eq!(settings.interface_preferences().unwrap().revision, 1);
        let snapshot = settings.interface_preferences().unwrap();
        settings
            .update(
                serde_json::from_value(json!({"expected_revision":0,"max_memory_mb":8192}))
                    .unwrap(),
            )
            .unwrap();
        assert_eq!(settings.current().unwrap().max_memory_mb, 8192);
        assert_eq!(settings.interface_preferences().unwrap(), snapshot);
        eprintln!(
            "interface preferences source_bytes={source_bytes} normalized_bytes={normalized_bytes} limit={MAX_INTERFACE_PREFERENCES_BYTES}"
        );
    }

    #[test]
    #[ignore = "bounded settings cost diagnostic; run explicitly with --nocapture"]
    fn maximum_envelope_read_and_route_write_cost() {
        for (label, value) in [
            ("small", envelope()),
            ("maximum_export", expanded_export().1),
        ] {
            let (_, settings) = store();
            replace(&settings, 0, Some(value)).unwrap();
            let read_started = std::time::Instant::now();
            for _ in 0..10 {
                settings.current().unwrap();
            }
            let read_time = read_started.elapsed();
            let write_started = std::time::Instant::now();
            for revision in 1..11 {
                settings
                    .update_interface_preferences(InterfacePreferencesUpdate {
                        expected_revision: revision,
                        change: InterfacePreferencesChange::Route {
                            route: Some(InterfaceRoute::Home {}),
                        },
                    })
                    .unwrap();
            }
            eprintln!(
                "{label}: 10 config reads {read_time:?}; 10 route writes {:?}",
                write_started.elapsed()
            );
            assert_eq!(settings.current().unwrap().revision, 0);
        }
    }
}
