//! Instance metadata. Names and serialized location hints never authorize files.

use crate::settings::InstanceSettings;
use serde::{Deserialize, Serialize};
use std::{fmt, str::FromStr};
use ts_rs::TS;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, TS)]
#[serde(try_from = "String", into = "String")]
#[ts(type = "string")]
pub struct InstanceId(String);

impl InstanceId {
    pub fn new() -> Self {
        Self(uuid::Uuid::new_v4().to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for InstanceId {
    fn default() -> Self {
        Self::new()
    }
}

impl FromStr for InstanceId {
    type Err = InstanceError;
    fn from_str(value: &str) -> InstanceResult<Self> {
        let parsed = uuid::Uuid::parse_str(value).map_err(|_| InstanceError::InvalidId)?;
        if parsed.is_nil() || parsed.to_string() != value {
            return Err(InstanceError::InvalidId);
        }
        Ok(Self(value.to_owned()))
    }
}

impl TryFrom<String> for InstanceId {
    type Error = InstanceError;
    fn try_from(value: String) -> InstanceResult<Self> {
        value.parse()
    }
}

impl From<InstanceId> for String {
    fn from(value: InstanceId) -> Self {
        value.0
    }
}

impl fmt::Display for InstanceId {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        out.write_str(&self.0)
    }
}

/// Retained view fields remain flat in JSON. The revision is a compare-and-set
/// precondition, not a launch/readiness projection.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct Instance {
    pub id: InstanceId,
    pub name: String,
    pub version_id: String,
    pub created_at: String,
    pub last_played_at: String,
    pub art_seed: u32,
    #[serde(flatten)]
    pub settings: InstanceSettings,
    /// Derived presentation only. The registry encoder excludes this field.
    #[serde(skip_deserializing, skip_serializing_if = "Option::is_none")]
    pub java_selection: Option<JavaSelection>,
    pub icon: String,
    pub accent: String,
    pub loader_key: String,
    pub minecraft_version: String,
    pub revision: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, TS)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum JavaSelection {
    Inherited,
    Component { component: String },
    Custom,
}

#[derive(Clone, Debug, Serialize)]
pub struct EnrichedInstance {
    #[serde(flatten)]
    pub instance: Instance,
    pub version_display: InstanceVersionDisplay,
    pub launchable: bool,
    pub launch_action: InstanceLaunchAction,
    pub status_detail: String,
    pub needs_install: String,
    pub install_target: Option<crate::install::model::InstallQueueInstallItemViewModel>,
    pub java_major: i32,
    pub saves_count: usize,
    pub mods_count: usize,
    pub resource_count: usize,
    pub shader_count: usize,
    pub counts_available: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct InstanceVersionDisplay {
    pub loader_key: String,
    pub loader_label: String,
    pub minecraft_label: String,
    pub loader_version_label: String,
    pub loader_detail_label: String,
    pub summary_label: String,
    pub supports_mods: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct InstanceLaunchAction {
    pub state_id: String,
    pub label: String,
    pub tone: String,
    pub launchable: bool,
    pub primary_action: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disabled_reason: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InstanceLifecycle {
    Reserved,
    Live,
    Deleting,
}

impl InstanceLifecycle {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Reserved => "reserved",
            Self::Live => "live",
            Self::Deleting => "deleting",
        }
    }
}

/// Private persisted metadata. A receipt locates an ownership witness for
/// verification; deserializing this record does not create a file capability.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstanceRecord {
    pub instance: Instance,
    pub revision: u64,
    pub lifecycle: InstanceLifecycle,
    pub library_id: String,
    pub directory_name: String,
    pub directory_receipt: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstancePatch {
    pub expected_revision: Option<u64>,
    pub name: Option<String>,
    /// Legacy edit clients send the expected existing version, never a retarget.
    pub version_id: Option<String>,
    pub art_seed: Option<u32>,
    pub max_memory_mb: Option<i32>,
    pub min_memory_mb: Option<i32>,
    pub java_path: Option<String>,
    pub window_width: Option<i32>,
    pub window_height: Option<i32>,
    pub jvm_preset: Option<String>,
    pub performance_mode: Option<String>,
    pub extra_jvm_args: Option<String>,
    pub auto_optimize: Option<bool>,
    pub icon: Option<String>,
    pub accent: Option<String>,
}

impl InstancePatch {
    pub(crate) fn apply(self, instance: &mut Instance) -> InstanceResult<()> {
        if self
            .expected_revision
            .is_some_and(|v| v != instance.revision)
            || self
                .version_id
                .as_ref()
                .is_some_and(|v| v != &instance.version_id)
        {
            return Err(InstanceError::Conflict);
        }
        if let Some(name) = self.name {
            instance.name = validate_name(&name)?;
        }
        if let Some(value) = self.art_seed {
            instance.art_seed = value;
        }
        if let Some(value) = self.icon {
            validate_label(&value, 1024)?;
            instance.icon = value;
        }
        if let Some(value) = self.accent {
            validate_label(&value, 64)?;
            instance.accent = value;
        }
        macro_rules! assign { ($($field:ident),+ $(,)?) => { $(if let Some(value) = self.$field { instance.settings.$field = value; })+ }; }
        assign!(
            max_memory_mb,
            min_memory_mb,
            java_path,
            window_width,
            window_height,
            jvm_preset,
            performance_mode,
            extra_jvm_args,
            auto_optimize
        );
        instance
            .settings
            .validate()
            .map_err(|_| InstanceError::InvalidSettings)?;
        Ok(())
    }
}

pub(crate) fn validate_name(value: &str) -> InstanceResult<String> {
    let value = value.trim();
    if value.is_empty() || value.chars().count() > 128 || value.chars().any(char::is_control) {
        return Err(InstanceError::InvalidName);
    }
    Ok(value.to_owned())
}

pub(crate) fn validate_label(value: &str, max_bytes: usize) -> InstanceResult<()> {
    if value.len() > max_bytes || value.chars().any(char::is_control) {
        return Err(InstanceError::InvalidInput);
    }
    Ok(())
}

#[derive(Debug, thiserror::Error)]
pub enum InstanceError {
    #[error("The instance identity is invalid.")]
    InvalidId,
    #[error("Enter an instance name between 1 and 128 characters.")]
    InvalidName,
    #[error("The instance request is invalid.")]
    InvalidInput,
    #[error("The instance settings are invalid.")]
    InvalidSettings,
    #[error("The instance was not found.")]
    NotFound,
    #[error("The instance changed. Refresh it before trying again.")]
    Conflict,
    #[error("An instance with this name already exists.")]
    NameConflict,
    #[error("The instance is busy. Wait for the current operation to finish.")]
    Busy,
    #[error("The instance library is unavailable. Reconnect the selected library.")]
    LibraryUnavailable,
    #[error("The instance directory could not be verified. Its files were preserved.")]
    DirectoryUnavailable,
    #[error("The operation requires settlement before this instance can be used.")]
    SettlementRequired,
    #[error("The application is shutting down.")]
    Closed,
    #[error("The instance operation was cancelled.")]
    Cancelled,
    #[error("The selected version is unavailable. Refresh the version list and try again.")]
    VersionUnavailable,
    #[error("Could not verify installed versions. Check the library folder and try again.")]
    InstalledVersionsDegraded,
    #[error(
        "Managed Performance state could not be verified for duplication. The source was preserved."
    )]
    ManagedDuplicateUnavailable,
    #[error("Instance content setup is unavailable. Refresh the plan and try again.")]
    SetupUnavailable,
    #[error("Instance metadata could not be read or saved.")]
    Storage(#[source] crate::storage::StorageError),
}

impl From<crate::storage::StorageError> for InstanceError {
    fn from(value: crate::storage::StorageError) -> Self {
        Self::Storage(value)
    }
}

impl From<crate::storage::rusqlite::Error> for InstanceError {
    fn from(value: crate::storage::rusqlite::Error) -> Self {
        Self::Storage(value.into())
    }
}

pub type InstanceResult<T> = Result<T, InstanceError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_rejects_names_paths_and_noncanonical_values() {
        for raw in [
            "Survival",
            "../mods",
            "/tmp/world",
            "00000000-0000-0000-0000-000000000000",
            "8F1D4319-DAD9-4E5A-880E-CC7F1E367DF7",
        ] {
            assert!(raw.parse::<InstanceId>().is_err());
            assert!(serde_json::from_value::<InstanceId>(serde_json::json!(raw)).is_err());
        }
        let id = InstanceId::new();
        assert_eq!(id.as_str().parse::<InstanceId>().unwrap(), id);
    }

    #[test]
    fn names_are_labels_and_need_not_be_directory_names() {
        assert_eq!(
            validate_name("  Weekend / friends  ").unwrap(),
            "Weekend / friends"
        );
        assert!(validate_name(" \n ").is_err());
        assert!(validate_name("world\0name").is_err());
    }
}
