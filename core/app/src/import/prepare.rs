//! Immutable, read-only input to the instance owner's retained publication.
//! Preparing one ordinary instance does not attest a complete profile cutover.

use std::{
    collections::{BTreeMap, BTreeSet},
    io::Read,
    sync::Arc,
};

use axial_fs::{Directory, DirectoryListingState, EntryKind, LeafName};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use super::{ImportBlocker, ImportError, ImportResult, Inventory, model::FileManifest};
use crate::{
    catalog::VersionDescriptor,
    files::{PortableName, ScopedDirectory, ScopedPath},
    instances::model::{Instance, InstanceId, validate_label, validate_name},
    settings::{ConfigView, EffectiveLaunchSettings, InstanceSettings, PreparedSettingsImport},
    tasks::CancellationToken,
};

/// Only an inventory can construct this input. It retains admitted source
/// capabilities and the exact preview, never opens a serialized source path.
#[derive(Clone)]
pub struct PreparedInstanceImport {
    inventory: Arc<Inventory>,
    legacy_id: String,
    source_id: String,
    was_last_instance: bool,
    instance: Instance,
    source_launch: EffectiveLaunchSettings,
    source: Directory,
    directories: Arc<[String]>,
    files: Arc<[FileManifest]>,
    history: super::history::PreparedHistory,
    performance_witness: Option<String>,
}

impl PreparedInstanceImport {
    pub(crate) fn legacy_id(&self) -> &str {
        &self.legacy_id
    }

    /// Physical source-profile identity, stable across a fresh read-only
    /// admission. A changed preview of this source must not create a duplicate.
    pub(crate) fn source_id(&self) -> &str {
        &self.source_id
    }

    pub(crate) fn fingerprint(&self) -> &str {
        self.inventory.fingerprint()
    }

    pub(crate) fn instance(&self) -> &Instance {
        &self.instance
    }

    pub(crate) fn was_last_instance(&self) -> bool {
        self.was_last_instance
    }

    pub(crate) fn source(&self) -> &Directory {
        &self.source
    }

    pub(crate) fn bind_history(
        &self,
        destination: &InstanceId,
    ) -> ImportResult<super::history::BoundHistory> {
        self.history.bind_instance(destination)
    }

    pub(crate) fn revalidate(&self) -> ImportResult<()> {
        self.inventory.revalidate()
    }

    /// This must precede any destination effect, including creating its
    /// instances parent. Independently admitted ancestors can share bytes.
    pub(crate) fn validate_destination_root(&self, destination: &Directory) -> ImportResult<()> {
        self.inventory.validate_destination_root(destination)
    }

    /// Zero dimensions, an automatic preset and an empty Java override retain
    /// inheritance semantics. Until an explicit override can represent those
    /// values, a different destination default makes this slice inadmissible.
    pub(crate) fn validate_destination(&self, config: &ConfigView) -> ImportResult<()> {
        let mut effective = self
            .instance
            .settings
            .effective(config)
            .map_err(|_| ImportError::InvalidData)?;
        effective.global_config_revision = self.source_launch.global_config_revision;
        if effective != self.source_launch {
            return Err(ImportError::InvalidData);
        }
        Ok(())
    }

    /// Called by the publisher before its durable ready point. Every ordinary
    /// user file, including unknown extensions, screenshots and logs, must be
    /// preserved. Extra files cannot be silently adopted into the receipt.
    pub(crate) fn verify_staged(
        &self,
        destination: &ScopedDirectory,
        cancel: &CancellationToken,
    ) -> ImportResult<()> {
        check_cancel(cancel)?;
        self.revalidate()?;
        let expected_files: BTreeMap<_, _> = self
            .files
            .iter()
            .map(|file| (file.relative.as_str(), file))
            .collect();
        let mut remaining_files = expected_files;
        let mut remaining_directories = self
            .directories
            .iter()
            .map(String::as_str)
            .collect::<std::collections::BTreeSet<_>>();
        verify_tree(
            destination.capability(),
            "",
            &mut remaining_directories,
            &mut remaining_files,
            cancel,
        )?;
        if !remaining_files.is_empty() || !remaining_directories.is_empty() {
            return Err(ImportError::InvalidData);
        }
        if let Some(witness) = &self.performance_witness {
            axial_performance::ManagedDuplicatePayload::verify_recovered(
                destination.capability(),
                Some(witness),
            )
            .map_err(managed_payload_error)?;
        }
        self.revalidate()?;
        check_cancel(cancel)
    }
}

impl Inventory {
    /// Admission for the first payload import slice. The whole profile keeps
    /// its remaining preference/account/browser obligations. Rules caches and
    /// operation journals still require their owning converters.
    pub(crate) fn prepare_instance(
        self: &Arc<Self>,
        expected_fingerprint: &str,
        legacy_id: &str,
    ) -> ImportResult<PreparedInstanceImport> {
        self.prepare_instance_with_rules(expected_fingerprint, legacy_id, None)
    }

    pub(super) fn prepare_instance_with_rules(
        self: &Arc<Self>,
        expected_fingerprint: &str,
        legacy_id: &str,
        rules: Option<&crate::performance::rules::CompletedRulesImport>,
    ) -> ImportResult<PreparedInstanceImport> {
        self.revalidate()?;
        if self.fingerprint() != expected_fingerprint {
            return Err(ImportError::SourceChanged);
        }
        let history = super::history::prepare_history_with_rules(self, rules)?;
        let payloads = prepare_payloads_with_rules(self, rules)?;
        let input = self.convert_instance_import(legacy_id, &history, &payloads)?;
        self.revalidate()?;
        Ok(PreparedInstanceImport {
            inventory: self.clone(),
            legacy_id: legacy_id.to_owned(),
            source_id: self.source_identity()?,
            was_last_instance: input.was_last_instance,
            instance: input.instance,
            source_launch: input.source_launch,
            source: input.source,
            directories: input.directories.into(),
            files: input.files.into(),
            history: input.history,
            performance_witness: payloads.performance_witnesses.get(legacy_id).cloned(),
        })
    }

    /// The preview and executable plan share this exact converter. Availability
    /// never bypasses the later source or destination publication fences.
    pub(super) fn convert_instance_import(
        &self,
        legacy_id: &str,
        history: &super::history::PreparedSourceHistory,
        payloads: &PreparedPayloads,
    ) -> ImportResult<InstanceImportInput> {
        let legacy = self
            .instances()
            .iter()
            .find(|instance| instance.legacy_id == legacy_id)
            .ok_or(ImportError::InvalidData)?;
        if legacy.blockers.iter().any(|blocker| {
            *blocker != ImportBlocker::InstanceMetadataRequiresConversion
                && !history.supports(blocker)
                && !payloads.supported_blockers.contains(blocker)
        }) || self.preview().blockers.iter().any(|blocker| {
            !matches!(
                blocker,
                ImportBlocker::CutoverNotImplemented | ImportBlocker::BrowserPreferencesRequired
            ) && !history.supports(blocker)
                && !payloads.supported_blockers.contains(blocker)
        }) {
            return Err(ImportError::InvalidData);
        }
        let registry: LegacyRegistry =
            serde_json::from_slice(&self.record_bytes("profile/instances.json")?)
                .map_err(|_| ImportError::InvalidData)?;
        if registry.schema_version != 3
            || !registry.pending_deletions.is_empty()
            || registry.instances.len() != self.instances().len()
            || registry.last_instance_id.as_ref().is_some_and(|selected| {
                !selected.is_empty()
                    && !self
                        .instances()
                        .iter()
                        .any(|item| &item.legacy_id == selected)
            })
        {
            return Err(ImportError::InvalidData);
        }
        let config: serde_json::Value =
            serde_json::from_slice(&self.record_bytes("profile/config.json")?)
                .map_err(|_| ImportError::InvalidData)?;
        let settings = crate::settings::prepare_legacy_import(&config)
            .map_err(|_| ImportError::InvalidData)?;
        validate_profile(self, &settings)?;
        let resolved = legacy
            .original
            .get("version_id")
            .and_then(serde_json::Value::as_str)
            .and_then(|id| self.resolved_version(id));
        let (instance, source_launch) = convert_instance(&legacy.original, &settings, resolved)?;
        let (source, directories, files) = self.instance_payload(legacy_id)?;
        // Portable aliases are rejected across files and directories, including
        // ancestors. The captured source is never renamed to make it fit.
        let mut paths = std::collections::BTreeSet::new();
        for path in directories
            .iter()
            .map(String::as_str)
            .chain(files.iter().map(|f| f.relative.as_str()))
        {
            let path = ScopedPath::new_exact(path).map_err(|_| ImportError::InvalidData)?;
            if !paths.insert(path.key()) {
                return Err(ImportError::InvalidData);
            }
        }
        Ok(InstanceImportInput {
            was_last_instance: registry.last_instance_id.as_deref() == Some(legacy_id),
            instance,
            source_launch,
            source,
            directories,
            files,
            history: history.for_instance(legacy_id)?,
        })
    }
}

/// Exact source records proven by their existing leaf owners. Witnesses retain
/// no effect authority; admission is sequential across the bounded inventory.
pub(super) struct PreparedPayloads {
    supported_blockers: BTreeSet<ImportBlocker>,
    performance_witnesses: BTreeMap<String, String>,
}

pub(super) fn prepare_payloads(inventory: &Inventory) -> ImportResult<PreparedPayloads> {
    prepare_payloads_with_rules(inventory, None)
}

pub(super) fn prepare_payloads_with_rules(
    inventory: &Inventory,
    rules: Option<&crate::performance::rules::CompletedRulesImport>,
) -> ImportResult<PreparedPayloads> {
    let ids: BTreeSet<_> = inventory
        .instances()
        .iter()
        .map(|item| item.legacy_id.as_str())
        .collect();
    let mut prepared = PreparedPayloads {
        supported_blockers: BTreeSet::from([
            ImportBlocker::ManagedStateRequiresConversion,
            ImportBlocker::ContentProvenanceRequiresConversion,
        ]),
        performance_witnesses: BTreeMap::new(),
    };
    let mut supported_records = BTreeSet::new();
    // History preparation has already verified this exact source receipt.
    // Cache activation is never a side effect of copying an instance.
    if rules.is_some() {
        supported_records.insert(super::rules::CACHE);
    }
    let mut managed = BTreeMap::<&str, Vec<&str>>::new();
    for record in inventory.obligations() {
        inventory.check_cancelled()?;
        let Some((id, relative)) = record
            .source_record
            .strip_prefix("instances/")
            .and_then(|path| path.split_once('/'))
        else {
            continue;
        };
        if !ids.contains(id) {
            continue;
        }
        match record.blocker {
            ImportBlocker::ManagedStateRequiresConversion
                if relative == "mods/.axial-lock.json"
                    || relative == "mods/.axial-performance"
                    || relative.starts_with("mods/.axial-performance/") =>
            {
                managed.entry(id).or_default().push(&record.source_record);
            }
            ImportBlocker::ContentProvenanceRequiresConversion
                if relative == "axial.content.json" =>
            {
                crate::content::provenance::validate_legacy_manifest(
                    &inventory.record_bytes(&record.source_record)?,
                )
                .map_err(|error| {
                    use crate::content::catalog::ContentError;
                    let failure = match error {
                        ContentError::Parse(_) => "parse",
                        ContentError::Provider(_) => "provider",
                        ContentError::ProviderMetadataInvalid(_) => "provider_metadata",
                        ContentError::Invalid(_) => "invalid",
                        ContentError::Unavailable => "unavailable",
                    };
                    tracing::warn!(failure, "Could not admit predecessor content provenance");
                    ImportError::InvalidData
                })?;
                supported_records.insert(record.source_record.as_str());
            }
            _ => {}
        }
    }
    for (id, records) in managed {
        inventory.check_cancelled()?;
        let payload =
            axial_performance::ManagedDuplicatePayload::admit(&inventory.instance_source(id)?)
                .map_err(managed_payload_error)?;
        prepared
            .performance_witnesses
            .insert(id.to_owned(), payload.witness());
        supported_records.extend(records);
    }
    for record in inventory.obligations() {
        if !supported_records.contains(record.source_record.as_str()) {
            prepared.supported_blockers.remove(&record.blocker);
        }
    }
    inventory.check_cancelled()?;
    Ok(prepared)
}

fn managed_payload_error(error: axial_performance::StateError) -> ImportError {
    use axial_performance::StateError;
    let failure = match error {
        StateError::Read(_) => "read",
        StateError::Parse(_) => "parse",
        StateError::InvalidFilename(_) => "filename",
        StateError::InvalidOwnership { .. } => "ownership",
        StateError::InvalidIntegrity { .. } => "integrity",
        StateError::InvalidRollbackId => "rollback_id",
        StateError::InvalidRollback(_) => "rollback",
        StateError::RollbackCandidateUnresumable => "rollback_candidate",
        StateError::InvalidState(_) => "state",
        StateError::Publication { .. } => "publication",
    };
    tracing::warn!(failure, "Could not admit predecessor Managed payload");
    ImportError::InvalidData
}

pub(super) struct InstanceImportInput {
    was_last_instance: bool,
    instance: Instance,
    source_launch: EffectiveLaunchSettings,
    source: Directory,
    directories: Vec<String>,
    files: Vec<FileManifest>,
    history: super::history::PreparedHistory,
}

impl InstanceImportInput {
    pub(super) fn loader_key(&self) -> &str {
        &self.instance.loader_key
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyRegistry {
    schema_version: u32,
    last_instance_id: Option<String>,
    pending_deletions: Vec<serde_json::Value>,
    instances: Vec<serde_json::Value>,
}

fn validate_profile(inventory: &Inventory, settings: &PreparedSettingsImport) -> ImportResult<()> {
    if matches!(
        axial_minecraft::runtime::parse_runtime_override(&settings.config.java_path_override),
        axial_minecraft::RuntimeOverride::ExecutablePath(_)
    ) {
        return Err(ImportError::InvalidData);
    }
    super::metadata::prepare_accounts(inventory, settings)?;
    Ok(())
}

fn convert_instance(
    value: &serde_json::Value,
    settings: &PreparedSettingsImport,
    resolved: Option<&VersionDescriptor>,
) -> ImportResult<(Instance, EffectiveLaunchSettings)> {
    let mut fields = value.as_object().cloned().ok_or(ImportError::InvalidData)?;
    if fields
        .remove("id")
        .and_then(|id| id.as_str().map(str::to_owned))
        .as_deref()
        .is_none_or(|id| !super::model::legacy_id(id))
        || fields.contains_key("revision")
    {
        return Err(ImportError::InvalidData);
    }
    // Deserialize the owning instance settings separately so its strict schema
    // rejects unknown retained fields even though Instance uses flatten.
    let mut instance_settings = serde_json::Map::new();
    for key in [
        "max_memory_mb",
        "min_memory_mb",
        "java_path",
        "window_width",
        "window_height",
        "jvm_preset",
        "performance_mode",
        "extra_jvm_args",
        "auto_optimize",
    ] {
        instance_settings.insert(
            key.into(),
            fields.remove(key).ok_or(ImportError::InvalidData)?,
        );
    }
    let instance_settings: InstanceSettings =
        serde_json::from_value(serde_json::Value::Object(instance_settings))
            .map_err(|_| ImportError::InvalidData)?;
    instance_settings
        .validate()
        .map_err(|_| ImportError::InvalidData)?;
    if matches!(
        axial_minecraft::runtime::parse_runtime_override(&instance_settings.java_path),
        axial_minecraft::RuntimeOverride::ExecutablePath(_)
    ) {
        return Err(ImportError::InvalidData);
    }
    const FIELDS: [&str; 10] = [
        "name",
        "version_id",
        "created_at",
        "last_played_at",
        "art_seed",
        "icon",
        "accent",
        "loader_key",
        "minecraft_version",
        "id",
    ];
    if fields.keys().any(|key| !FIELDS.contains(&key.as_str())) {
        return Err(ImportError::InvalidData);
    }
    fields.insert(
        "id".into(),
        serde_json::Value::String(InstanceId::new().to_string()),
    );
    fields.insert("revision".into(), serde_json::Value::from(0));
    fields.extend(
        serde_json::to_value(&instance_settings)
            .map_err(|_| ImportError::InvalidData)?
            .as_object()
            .ok_or(ImportError::InvalidData)?
            .clone(),
    );
    let mut instance: Instance = serde_json::from_value(serde_json::Value::Object(fields))
        .map_err(|_| ImportError::InvalidData)?;
    if validate_name(&instance.name).map_err(|_| ImportError::InvalidData)? != instance.name {
        return Err(ImportError::InvalidData);
    }
    normalize_selection(&mut instance, resolved)?;
    validate_label(&instance.icon, 1024).map_err(|_| ImportError::InvalidData)?;
    validate_label(&instance.accent, 64).map_err(|_| ImportError::InvalidData)?;
    for timestamp in [&instance.created_at, &instance.last_played_at] {
        if timestamp.len() > 64
            || (!timestamp.is_empty() && chrono::DateTime::parse_from_rfc3339(timestamp).is_err())
        {
            return Err(ImportError::InvalidData);
        }
    }
    if instance.created_at.is_empty() {
        return Err(ImportError::InvalidData);
    }
    // An instance-only import cannot inherit a different destination profile's
    // launch preferences. Preserve the source's effective launch values.
    let effective = instance
        .settings
        .effective(&settings.config)
        .map_err(|_| ImportError::InvalidData)?;
    instance.settings.max_memory_mb = effective.max_memory_mb;
    instance.settings.min_memory_mb = effective.min_memory_mb;
    instance.settings.java_path = effective.java_path.clone();
    instance.settings.window_width = effective.window_width;
    instance.settings.window_height = effective.window_height;
    instance.settings.jvm_preset = effective.jvm_preset.as_str().to_owned();
    instance.settings.performance_mode = effective.performance_mode.as_str().to_owned();
    Ok((instance, effective))
}

/// Only exact empty declarations need provider evidence. Missing, mistyped,
/// contradictory or reserved loader identities are not normalization candidates.
pub(super) fn unresolved_version_id(value: &serde_json::Value) -> Option<&str> {
    let id = value.get("version_id")?.as_str()?;
    let loader = value.get("loader_key")?.as_str()?;
    let minecraft = value.get("minecraft_version")?.as_str()?;
    (minecraft.is_empty()
        && matches!(loader, "" | "vanilla")
        && !id.starts_with("loader-v2-")
        && validate_version_coordinate(id).is_ok())
    .then_some(id)
}

fn normalize_selection(
    instance: &mut Instance,
    resolved: Option<&VersionDescriptor>,
) -> ImportResult<()> {
    validate_version_coordinate(&instance.version_id)?;
    if !instance.minecraft_version.is_empty() {
        validate_version_coordinate(&instance.minecraft_version)?;
    }
    if let Ok(identity) =
        axial_minecraft::loaders::api::decode_installed_version_id(&instance.version_id)
    {
        validate_version_coordinate(identity.minecraft_version())?;
        if (!instance.loader_key.is_empty()
            && identity.component_id().short_key() != instance.loader_key)
            || (!instance.minecraft_version.is_empty()
                && identity.minecraft_version() != instance.minecraft_version)
        {
            return Err(ImportError::InvalidData);
        }
        instance.loader_key = identity.component_id().short_key().to_owned();
        instance.minecraft_version = identity.minecraft_version().to_owned();
        return Ok(());
    }
    if instance.version_id.starts_with("loader-v2-")
        || !matches!(instance.loader_key.as_str(), "" | "vanilla")
    {
        return Err(ImportError::InvalidData);
    }
    if instance.minecraft_version.is_empty()
        && resolved.is_some_and(|descriptor| descriptor.id() == instance.version_id)
    {
        instance.minecraft_version = instance.version_id.clone();
    }
    if instance.version_id != instance.minecraft_version {
        return Err(ImportError::InvalidData);
    }
    instance.loader_key = "vanilla".to_owned();
    Ok(())
}

fn validate_version_coordinate(value: &str) -> ImportResult<()> {
    // Preserve the predecessor's ASCII schema, then require the install leaf's
    // actual version and JSON filename shapes. Legacy IDs of 251–256 bytes
    // cannot fit the destination JSON filename and remain unavailable.
    if value.is_empty()
        || value.len() > 256
        || matches!(value, "." | "..")
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b'+'))
        || PortableName::new_exact(value).is_err()
        || PortableName::new_exact(&format!("{value}.json")).is_err()
    {
        return Err(ImportError::InvalidData);
    }
    Ok(())
}

fn verify_tree<'a>(
    directory: &Directory,
    prefix: &str,
    remaining_directories: &mut std::collections::BTreeSet<&'a str>,
    remaining_files: &mut BTreeMap<&'a str, &'a FileManifest>,
    cancel: &CancellationToken,
) -> ImportResult<()> {
    check_cancel(cancel)?;
    let revision = directory.revision()?;
    let listing = directory.entries(axial_fs::MAX_DIRECTORY_LIST_ENTRIES)?;
    if listing.state() != DirectoryListingState::Complete {
        return Err(ImportError::LimitExceeded);
    }
    for entry in listing.entries() {
        check_cancel(cancel)?;
        let leaf = entry.utf8_name().ok_or(ImportError::InvalidData)?;
        PortableName::new_exact(leaf).map_err(|_| ImportError::InvalidData)?;
        let path = format!("{prefix}{leaf}");
        match entry.kind() {
            EntryKind::Directory if remaining_directories.remove(path.as_str()) => {
                verify_tree(
                    &directory.open_observed_directory(entry)?,
                    &format!("{path}/"),
                    remaining_directories,
                    remaining_files,
                    cancel,
                )?;
            }
            EntryKind::File => {
                let expected = remaining_files
                    .remove(path.as_str())
                    .ok_or(ImportError::InvalidData)?;
                let file = directory
                    .open_file(&LeafName::new(leaf).map_err(|_| ImportError::InvalidData)?)?;
                let revision = file.revision()?;
                if revision.size() != expected.size {
                    return Err(ImportError::InvalidData);
                }
                let mut reader = file.reader(expected.size)?;
                let mut hash = Sha256::new();
                let mut buffer = [0_u8; 128 * 1024];
                loop {
                    check_cancel(cancel)?;
                    let read = reader.read(&mut buffer)?;
                    if read == 0 {
                        break;
                    }
                    hash.update(&buffer[..read]);
                }
                reader.finish()?;
                file.validate_revision(&revision)?;
                if hex::encode(hash.finalize()) != expected.sha256 {
                    return Err(ImportError::InvalidData);
                }
            }
            _ => return Err(ImportError::InvalidData),
        }
    }
    directory.validate_revision(&revision)?;
    check_cancel(cancel)
}

fn check_cancel(cancel: &CancellationToken) -> ImportResult<()> {
    if cancel.is_cancelled() {
        Err(ImportError::Cancelled)
    } else {
        Ok(())
    }
}
