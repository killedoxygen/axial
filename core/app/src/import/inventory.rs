use super::{ImportError, ImportResult, model::*};
use crate::{
    catalog::{Catalog, CatalogError, VersionDescriptor},
    tasks::CancellationToken,
};
use axial_fs::{
    Directory, DirectoryListingState, EntryKind, FileCapability, FileRevision, LeafName,
    MAX_DIRECTORY_LIST_ENTRIES,
};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::Read,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

const RECORD_LIMIT: u64 = 16 * 1024 * 1024;
const RECORDS_BYTE_LIMIT: u64 = 64 * 1024 * 1024;
const RECORDS_LIMIT: usize = 4096;
const MAX_DEPTH: usize = 64;
const REJECTION_STREAK_RECORD: &str = "profile/state/persisted-state-rejection-streaks.json";

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyRejectionStreaks {
    schema: String,
    entries: Vec<LegacyRejectionStreak>,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyRejectionStreak {
    store: String,
    record_id: String,
    physical_identity: String,
    consecutive_startups: u8,
}

fn valid_rejection_streaks(bytes: &[u8]) -> bool {
    if bytes.len() > 32 * 1024 {
        return false;
    }
    let Ok(snapshot) = serde_json::from_slice::<LegacyRejectionStreaks>(bytes) else {
        return false;
    };
    if snapshot.schema != "axial.state.persisted_state_rejection_streaks.v1"
        || snapshot.entries.len() > 8
    {
        return false;
    }
    let mut previous = None;
    for entry in &snapshot.entries {
        let valid_identity =
            entry
                .physical_identity
                .strip_prefix("sha256.")
                .is_some_and(|digest| {
                    let mut groups = digest.split('.');
                    (0..8).all(|_| {
                        groups.next().is_some_and(|group| {
                            group.len() == 8
                                && group.bytes().all(|byte| {
                                    byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)
                                })
                        })
                    }) && groups.next().is_none()
                });
        if entry.store != "benchmark_suite_driver"
            || !entry
                .record_id
                .strip_prefix("benchmark-suite-driver-")
                .is_some_and(legacy_id)
            || !valid_identity
            || !(1..=3).contains(&entry.consecutive_startups)
            || previous.is_some_and(|previous| previous >= entry.record_id.as_str())
        {
            return false;
        }
        previous = Some(entry.record_id.as_str());
    }
    true
}

#[derive(Clone, Copy)]
pub struct CaptureLimits {
    pub files: usize,
    pub bytes: u64,
}
impl Default for CaptureLimits {
    fn default() -> Self {
        Self {
            files: 1_000_000,
            bytes: 256 * 1024 * 1024 * 1024,
        }
    }
}

/// This wrapper provides no write, removal or raw-path access. The admitting
/// owner keeps its replacement root session alive for the preview lifetime.
#[derive(Clone)]
pub struct ReadOnlySource {
    directory: Directory,
    root_pin: Option<crate::library::ApplicationRootPin>,
}
impl ReadOnlySource {
    pub fn from_admitted_directory(directory: Directory) -> Self {
        Self {
            directory,
            root_pin: None,
        }
    }

    /// Only the native OS-selection boundary supplies this path. Retaining the
    /// replacement root does not acquire a session or write into the old root.
    pub fn from_native_selection(
        root: crate::library::ApplicationRootPin,
        path: &Path,
    ) -> ImportResult<Self> {
        let directory = root.admit_native_directory(path)?;
        Ok(Self {
            directory,
            root_pin: Some(root),
        })
    }
}

struct CapturedFile {
    pub manifest: FileManifest,
    pub capability: FileCapability,
    pub revision: FileRevision,
}

pub struct Inventory {
    sources: Vec<ReadOnlySource>,
    files: Vec<CapturedFile>,
    directories: Vec<(String, Directory, axial_fs::DirectoryRevision)>,
    records: BTreeMap<String, Value>,
    resolved_versions: BTreeMap<String, VersionDescriptor>,
    instances: Vec<LegacyInstance>,
    obligations: Vec<RetainedObligation>,
    preview: ImportPreview,
    limits: CaptureLimits,
    cancelled: Arc<AtomicBool>,
    record_bytes: u64,
}

impl Inventory {
    /// `external_instances` contains independently admitted source directories.
    /// Neither config.library_dir nor a journal path is followed.
    pub fn capture(
        profile: &ReadOnlySource,
        external_instances: &BTreeMap<String, ReadOnlySource>,
    ) -> ImportResult<Self> {
        Self::capture_controlled(
            profile,
            external_instances,
            CaptureLimits::default(),
            Arc::new(AtomicBool::new(false)),
        )
    }

    pub fn capture_controlled(
        profile: &ReadOnlySource,
        external_instances: &BTreeMap<String, ReadOnlySource>,
        limits: CaptureLimits,
        cancelled: Arc<AtomicBool>,
    ) -> ImportResult<Self> {
        let mut result = Self {
            sources: std::iter::once(profile.clone())
                .chain(external_instances.values().cloned())
                .collect(),
            files: vec![],
            directories: vec![],
            records: BTreeMap::new(),
            resolved_versions: BTreeMap::new(),
            instances: vec![],
            obligations: vec![],
            preview: ImportPreview {
                cutover_available: false,
                fingerprint: String::new(),
                metadata_import_id: String::new(),
                metadata_import_available: false,
                skin_import_id: String::new(),
                rules_import_id: String::new(),
                rules_import_available: false,
                skin_import_available: false,
                instances: vec![],
                file_count: 0,
                byte_count: 0,
                offline_account_count: 0,
                microsoft_reauthentication_count: 0,
                saved_skin_count: 0,
                retained_obligation_count: 0,
                retained_records: vec![],
                blockers: vec![
                    ImportBlocker::CutoverNotImplemented,
                    ImportBlocker::BrowserPreferencesRequired,
                ],
            },
            limits,
            cancelled,
            record_bytes: 0,
        };
        result.capture_profile(&profile.directory)?;
        result.parse_profile()?;
        let internal = optional_directory(&profile.directory, "instances")?;
        if let Some(root) = &internal {
            result.observe_directory(root, "instances")?;
            for entry in result.entries(root)?.entries() {
                if !entry.utf8_name().is_some_and(|id| {
                    result
                        .instances
                        .iter()
                        .any(|instance| instance.legacy_id == id)
                }) {
                    result.retain(
                        &format!("instances/{:?}", entry.name()),
                        None,
                        ImportBlocker::UnknownRetainedRecord,
                    );
                }
            }
        }
        for index in 0..result.instances.len() {
            let id = result.instances[index].legacy_id.clone();
            let source = if let Some(source) = external_instances.get(&id) {
                Some(source.directory.clone())
            } else if let Some(root) = &internal {
                optional_directory(root, &id)?
            } else {
                None
            };
            if let Some(source) = source {
                result.capture_tree(&source, &format!("instances/{id}"), 0)?;
            } else {
                push_unique(
                    &mut result.instances[index].blockers,
                    ImportBlocker::MissingInstanceSource,
                );
            }
        }
        if external_instances
            .keys()
            .any(|id| !result.instances.iter().any(|item| &item.legacy_id == id))
        {
            return Err(ImportError::InvalidData);
        }
        result.classify_obligations();
        result.finalize()?;
        Ok(result)
    }

    pub fn preview(&self) -> ImportPreview {
        self.preview.clone()
    }

    /// Derive display eligibility before native selection's final in-memory
    /// swap. This performs source/metadata I/O and belongs in the existing
    /// blocking capture worker, never under the native selection-state lock.
    pub fn resolve_rules_preview(
        mut self,
        rules: &crate::performance::rules::PerformanceRules,
    ) -> ImportResult<Self> {
        self.preview = self.rules_preview(rules)?;
        Ok(self)
    }

    pub(super) fn rules_preview(
        &self,
        rules: &crate::performance::rules::PerformanceRules,
    ) -> ImportResult<ImportPreview> {
        self.revalidate()?;
        let completed = rules
            .completed_import(&self.source_identity()?, self.fingerprint())
            .map_err(|_| ImportError::Unavailable)?;
        let preview = self.preview_with_rules(completed.as_ref());
        self.revalidate()?;
        Ok(preview)
    }

    /// Fill genuinely absent provider-selection fields before admission. This
    /// does not adopt source installation bytes or attest executable readiness.
    /// Provider failure leaves unresolved rows unavailable, not the whole import.
    pub async fn resolve_versions(
        self,
        catalog: &Catalog,
        cancel: &CancellationToken,
    ) -> ImportResult<Self> {
        if cancel.is_cancelled() {
            return Err(ImportError::Cancelled);
        }
        self.check_cancelled()?;
        let ids = self
            .instances
            .iter()
            .filter_map(|instance| super::prepare::unresolved_version_id(&instance.original))
            .map(str::to_owned)
            .collect::<BTreeSet<_>>();
        if cancel.is_cancelled() {
            return Err(ImportError::Cancelled);
        }
        self.check_cancelled()?;
        if ids.is_empty() {
            return Ok(self);
        }
        let inventory = tokio::task::spawn_blocking(move || {
            self.revalidate()?;
            Ok::<_, ImportError>(self)
        })
        .await
        .map_err(|_| ImportError::Unavailable)??;
        let resolved = match catalog.resolve_fresh_selections(&ids, cancel).await {
            Ok(resolved) => resolved,
            Err(CatalogError::Cancelled) => return Err(ImportError::Cancelled),
            Err(error) => {
                tracing::warn!(
                    failure = ?error.failure(),
                    "Could not resolve predecessor version selections"
                );
                BTreeMap::new()
            }
        };
        let cancel = cancel.clone();
        tokio::task::spawn_blocking(move || {
            if cancel.is_cancelled() {
                return Err(ImportError::Cancelled);
            }
            let mut inventory = inventory;
            if !resolved.is_empty() || !inventory.resolved_versions.is_empty() {
                inventory.resolved_versions = resolved;
                inventory.refresh_instance_preview();
            }
            inventory.revalidate()?;
            if cancel.is_cancelled() {
                return Err(ImportError::Cancelled);
            }
            Ok(inventory)
        })
        .await
        .map_err(|_| ImportError::Unavailable)?
    }

    pub(super) fn resolved_version(&self, id: &str) -> Option<&VersionDescriptor> {
        self.resolved_versions.get(id)
    }

    pub(super) fn admit_snapshot(mut self) -> ImportResult<Self> {
        self.check_cancelled()?;
        // Capture cancellation ends at admission. Accepted copies retain this
        // snapshot and observe their own publication task's cancellation.
        self.cancelled = Arc::new(AtomicBool::new(false));
        Ok(self)
    }
    pub(super) fn fingerprint(&self) -> &str {
        &self.preview.fingerprint
    }
    pub fn instances(&self) -> &[LegacyInstance] {
        &self.instances
    }
    pub fn obligations(&self) -> &[RetainedObligation] {
        &self.obligations
    }

    pub(super) fn file_manifests(&self) -> impl Iterator<Item = &FileManifest> {
        self.files.iter().map(|file| &file.manifest)
    }

    pub(super) fn directory_names(&self) -> impl Iterator<Item = &str> {
        self.directories.iter().map(|(name, _, _)| name.as_str())
    }

    /// Converters can reread original bytes using a captured source file name.
    /// A serialized source path or a JSON pointer never grants file authority.
    pub fn record_bytes(&self, relative: &str) -> ImportResult<Vec<u8>> {
        // finalize sorts captures before any converter runs. A wardrobe can
        // contain thousands of PNGs; do not scan the whole profile per read.
        let index = self
            .files
            .binary_search_by(|file| file.manifest.relative.as_str().cmp(relative))
            .map_err(|_| ImportError::InvalidData)?;
        let file = &self.files[index];
        file.capability
            .validate_revision(&file.revision)
            .map_err(|_| ImportError::SourceChanged)?;
        let bytes = file.capability.read_bounded(RECORD_LIMIT)?;
        if hex::encode(Sha256::digest(&bytes)) != file.manifest.sha256 {
            return Err(ImportError::SourceChanged);
        }
        Ok(bytes)
    }

    pub fn revalidate(&self) -> ImportResult<()> {
        self.check_cancelled()?;
        for source in &self.sources {
            if let Some(pin) = &source.root_pin {
                pin.revalidate().map_err(|_| ImportError::Unavailable)?;
            }
        }
        for (_, directory, revision) in &self.directories {
            self.check_cancelled()?;
            directory
                .validate_revision(revision)
                .map_err(|_| ImportError::SourceChanged)?;
        }
        for file in &self.files {
            self.check_cancelled()?;
            file.capability
                .validate_revision(&file.revision)
                .map_err(|_| ImportError::SourceChanged)?;
        }
        Ok(())
    }

    pub(super) fn source_identity(&self) -> ImportResult<String> {
        let (_, profile, _) = self
            .directories
            .iter()
            .find(|(relative, _, _)| relative == "profile")
            .ok_or(ImportError::InvalidData)?;
        Ok(hex::encode(profile.identity()?.filesystem_witness()))
    }

    pub(super) fn validate_destination_root(&self, destination: &Directory) -> ImportResult<()> {
        self.revalidate()?;
        for (_, source, _) in &self.directories {
            if source.overlaps(destination)? {
                return Err(ImportError::InvalidData);
            }
        }
        self.revalidate()
    }

    pub(super) fn instance_payload(
        &self,
        id: &str,
    ) -> ImportResult<(Directory, Vec<String>, Vec<FileManifest>)> {
        let root = format!("instances/{id}");
        let prefix = format!("{root}/");
        let source = self.instance_source(id)?;
        let directories = self
            .directories
            .iter()
            .filter_map(|(relative, _, _)| relative.strip_prefix(&prefix).map(str::to_owned))
            .collect();
        let files = self
            .files
            .iter()
            .filter_map(|file| {
                file.manifest
                    .relative
                    .strip_prefix(&prefix)
                    .map(|relative| FileManifest {
                        relative: relative.to_owned(),
                        size: file.manifest.size,
                        sha256: file.manifest.sha256.clone(),
                    })
            })
            .collect();
        Ok((source, directories, files))
    }

    pub(super) fn instance_source(&self, id: &str) -> ImportResult<Directory> {
        let root = format!("instances/{id}");
        let index = self
            .directories
            .binary_search_by(|(relative, _, _)| relative.cmp(&root))
            .map_err(|_| ImportError::InvalidData)?;
        Ok(self.directories[index].1.clone())
    }

    pub(super) fn check_cancelled(&self) -> ImportResult<()> {
        if self.cancelled.load(Ordering::Acquire) {
            Err(ImportError::Cancelled)
        } else {
            Ok(())
        }
    }
    fn observe_directory(&mut self, directory: &Directory, relative: &str) -> ImportResult<()> {
        self.check_cancelled()?;
        if self.directories.len() >= self.limits.files {
            return Err(ImportError::LimitExceeded);
        }
        self.directories
            .push((relative.into(), directory.clone(), directory.revision()?));
        Ok(())
    }
    fn entries(&self, directory: &Directory) -> ImportResult<axial_fs::DirectoryListing> {
        // The capture budget spans the whole tree; one native listing has its
        // own smaller bound. Truncated listings still fail the entire preview.
        let entries = directory.entries(self.limits.files.min(MAX_DIRECTORY_LIST_ENTRIES))?;
        if entries.state() != DirectoryListingState::Complete {
            return Err(ImportError::LimitExceeded);
        }
        Ok(entries)
    }

    fn capture_profile(&mut self, profile: &Directory) -> ImportResult<()> {
        self.observe_directory(profile, "profile")?;
        for entry in self.entries(profile)?.entries() {
            let Some(leaf) = entry.utf8_name() else {
                self.retain(
                    &format!("profile/{:?}", entry.name()),
                    None,
                    ImportBlocker::UnsafeFile,
                );
                continue;
            };
            // Guardian histories and replaceable caches are intentionally not
            // imported. Keyring is never opened and baseline lease never touched.
            if matches!(
                leaf,
                ".axial-root.lease" | "guardian" | "library" | "runtimes" | "updates" | "instances"
            ) {
                continue;
            }
            let relative = format!("profile/{leaf}");
            match entry.kind() {
                EntryKind::Directory => {
                    if !matches!(leaf, "skins" | "performance" | "benchmarks" | "state") {
                        self.retain(&relative, None, ImportBlocker::UnknownRetainedRecord);
                    }
                    self.capture_tree(&profile.open_observed_directory(entry)?, &relative, 0)?
                }
                EntryKind::File => self.capture_file(
                    profile.open_file(&name(leaf)?)?,
                    &relative,
                    leaf.ends_with(".json"),
                )?,
                _ => self.retain(&relative, None, ImportBlocker::UnsafeFile),
            }
        }
        for leaf in ["config.json", "accounts.json", "instances.json"] {
            if !self
                .files
                .iter()
                .any(|file| file.manifest.relative == format!("profile/{leaf}"))
            {
                self.block(ImportBlocker::MissingRequiredRecord);
            }
        }
        Ok(())
    }

    fn capture_file(
        &mut self,
        capability: FileCapability,
        relative: &str,
        record: bool,
    ) -> ImportResult<()> {
        self.check_cancelled()?;
        if self.files.len() >= self.limits.files {
            return Err(ImportError::LimitExceeded);
        }
        let revision = capability.revision()?;
        if record && revision.size() <= RECORD_LIMIT {
            self.record_bytes = self
                .record_bytes
                .checked_add(revision.size())
                .ok_or(ImportError::LimitExceeded)?;
            if self.record_bytes > RECORDS_BYTE_LIMIT || self.records.len() >= RECORDS_LIMIT {
                return Err(ImportError::LimitExceeded);
            }
        }
        self.preview.byte_count = self
            .preview
            .byte_count
            .checked_add(revision.size())
            .ok_or(ImportError::LimitExceeded)?;
        if self.preview.byte_count > self.limits.bytes {
            return Err(ImportError::LimitExceeded);
        }
        let mut reader = capability.reader(revision.size())?;
        let mut hash = Sha256::new();
        let mut bytes = Vec::new();
        let mut buffer = [0_u8; 128 * 1024];
        loop {
            self.check_cancelled()?;
            let count = reader.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            hash.update(&buffer[..count]);
            if record && revision.size() <= RECORD_LIMIT {
                bytes.extend_from_slice(&buffer[..count]);
            }
        }
        reader.finish()?;
        capability
            .validate_revision(&revision)
            .map_err(|_| ImportError::SourceChanged)?;
        if record {
            // Guardian eligibility is excluded, but its exact source schema must
            // be checked before JSON normalization can hide duplicate fields.
            if relative == REJECTION_STREAK_RECORD && !valid_rejection_streaks(&bytes) {
                self.retain(relative, None, ImportBlocker::UnsupportedSchema);
            }
            if revision.size() <= RECORD_LIMIT {
                if let Ok(value) = serde_json::from_slice(&bytes) {
                    self.records.insert(relative.into(), value);
                } else {
                    self.retain(relative, None, ImportBlocker::UnsupportedSchema);
                }
            } else {
                self.retain(relative, None, ImportBlocker::UnsupportedSchema);
            }
        }
        self.files.push(CapturedFile {
            manifest: FileManifest {
                relative: relative.into(),
                size: revision.size(),
                sha256: hex::encode(hash.finalize()),
            },
            capability,
            revision,
        });
        Ok(())
    }

    fn capture_tree(
        &mut self,
        directory: &Directory,
        relative: &str,
        depth: usize,
    ) -> ImportResult<()> {
        if depth > MAX_DEPTH {
            return Err(ImportError::LimitExceeded);
        }
        self.observe_directory(directory, relative)?;
        for entry in self.entries(directory)?.entries() {
            self.check_cancelled()?;
            let Some(leaf) = entry.utf8_name() else {
                self.retain(
                    &format!("{relative}/{:?}", entry.name()),
                    None,
                    ImportBlocker::UnsafeFile,
                );
                continue;
            };
            if relative == "profile/state" && leaf == "known-good" {
                continue;
            }
            let path = format!("{relative}/{leaf}");
            match entry.kind() {
                EntryKind::Directory => {
                    let reserved = leaf.to_ascii_lowercase();
                    if reserved == ".axial-performance" || reserved.starts_with(".axial-lock") {
                        self.retain(&path, None, ImportBlocker::ManagedStateRequiresConversion);
                    } else if reserved.starts_with(".axial") || reserved == "axial.content.json" {
                        self.retain(
                            &path,
                            None,
                            ImportBlocker::ContentProvenanceRequiresConversion,
                        );
                    } else if relative == "profile/state" {
                        self.retain(&path, None, ImportBlocker::UnknownRetainedRecord);
                    }
                    self.capture_tree(&directory.open_observed_directory(entry)?, &path, depth + 1)?
                }
                EntryKind::File => {
                    // Performance intent/park records need not end in .json.
                    let record = ((path.starts_with("profile/")
                        || path.contains("/.axial-performance/"))
                        && leaf.ends_with(".json"))
                        || leaf.starts_with(".axial")
                        || leaf == "axial.content.json";
                    self.capture_file(directory.open_file(&name(leaf)?)?, &path, record)?;
                }
                _ => self.retain(&path, None, ImportBlocker::UnsafeFile),
            }
        }
        Ok(())
    }

    fn parse_profile(&mut self) -> ImportResult<()> {
        if let Some(registry) = self.records.get("profile/instances.json").cloned() {
            if registry.get("schema_version").and_then(Value::as_u64) != Some(3) {
                self.retain(
                    "profile/instances.json",
                    Some(registry),
                    ImportBlocker::UnsupportedSchema,
                );
            } else if let Some(instances) = registry.get("instances").and_then(Value::as_array) {
                if instances.len() > RECORDS_LIMIT {
                    return Err(ImportError::LimitExceeded);
                }
                let mut seen = BTreeSet::new();
                for original in instances {
                    let id = text(original, "id").ok_or(ImportError::InvalidData)?;
                    if !legacy_id(id) || !seen.insert(id.to_owned()) {
                        return Err(ImportError::InvalidData);
                    }
                    let mut blockers = vec![ImportBlocker::InstanceMetadataRequiresConversion];
                    if !matches!(
                        text(original, "loader_key"),
                        Some("vanilla" | "fabric" | "quilt" | "forge" | "neoforge" | "")
                    ) {
                        blockers.push(ImportBlocker::UnsupportedLoader);
                    }
                    self.instances.push(LegacyInstance {
                        legacy_id: id.into(),
                        original: original.clone(),
                        blockers,
                    });
                }
            } else {
                self.retain(
                    "profile/instances.json",
                    Some(registry),
                    ImportBlocker::UnsupportedSchema,
                );
            }
        }
        self.parse_accounts();
        if let Some(config) = self.records.get("profile/config.json").cloned() {
            if crate::settings::prepare_legacy_import(&config).is_err() {
                self.retain(
                    "profile/config.json",
                    Some(config),
                    ImportBlocker::RetainedPreferenceRequiresConversion,
                );
            }
        }
        if let Some(skins) = self.records.get("profile/skins/index.json").cloned() {
            if text(&skins, "schema") != Some("axial.skins.saved")
                || skins.get("schema_version").and_then(Value::as_u64) != Some(3)
            {
                self.retain(
                    "profile/skins/index.json",
                    Some(skins),
                    ImportBlocker::UnsupportedSchema,
                );
            } else {
                self.preview.saved_skin_count = skins
                    .get("skins")
                    .and_then(Value::as_array)
                    .map_or(0, Vec::len);
            }
        }
        Ok(())
    }

    fn parse_accounts(&mut self) {
        let Some(accounts) = self.records.get("profile/accounts.json").cloned() else {
            return;
        };
        if text(&accounts, "schema") != Some("axial.accounts")
            || accounts.get("schema_version").and_then(Value::as_u64) != Some(1)
        {
            self.retain(
                "profile/accounts.json",
                Some(accounts),
                ImportBlocker::UnsupportedSchema,
            );
            return;
        }
        let Some(records) = accounts.get("accounts").and_then(Value::as_array) else {
            self.retain(
                "profile/accounts.json",
                Some(accounts),
                ImportBlocker::UnsupportedSchema,
            );
            return;
        };
        let mut seen = BTreeSet::new();
        let mut destinations = BTreeSet::new();
        let mut invalid = false;
        for (index, record) in records.iter().enumerate() {
            let valid_identity =
                text(record, "account_id").is_some_and(|id| seen.insert(id.to_owned()));
            match text(record, "kind") {
                Some("offline") => {
                    self.preview.offline_account_count += 1;
                }
                Some("microsoft") => {
                    self.preview.microsoft_reauthentication_count += 1;
                }
                _ => (),
            };
            let valid = super::metadata::destination_account_id(record)
                .is_ok_and(|id| destinations.insert(id));
            if !valid_identity || !valid {
                invalid = true;
                self.retain(
                    &format!("profile/accounts.json#/accounts/{index}"),
                    Some(record.clone()),
                    ImportBlocker::AccountRequiresConversion,
                );
            }
        }
        if let Some(selected) = accounts
            .get("active_account_id")
            .filter(|value| !value.is_null())
        {
            if !selected.as_str().is_some_and(|id| seen.contains(id)) {
                invalid = true;
                self.retain(
                    "profile/accounts.json#/active_account_id",
                    Some(selected.clone()),
                    ImportBlocker::AccountRequiresConversion,
                );
            }
        }
        if !invalid && super::metadata::convert_accounts(accounts.clone()).is_err() {
            self.retain(
                "profile/accounts.json",
                Some(accounts),
                ImportBlocker::AccountRequiresConversion,
            );
        }
    }

    fn classify_obligations(&mut self) {
        if let Some(registry) = self.records.get("profile/instances.json").cloned() {
            if let Some(records) = registry.get("pending_deletions").and_then(Value::as_array) {
                for (index, record) in records.iter().enumerate() {
                    self.retain(
                        &format!("profile/instances.json#/pending_deletions/{index}"),
                        Some(record.clone()),
                        ImportBlocker::PendingDeletion,
                    );
                }
            } else {
                self.retain(
                    "profile/instances.json#/pending_deletions",
                    Some(registry),
                    ImportBlocker::UnsupportedSchema,
                );
            }
        }
        if let Some(journal) = self
            .records
            .get("profile/state/operation-journals.json")
            .cloned()
        {
            if journal.get("schema").and_then(Value::as_str)
                != Some("axial.state.operation_journals.v10")
            {
                self.retain(
                    "profile/state/operation-journals.json",
                    Some(journal),
                    ImportBlocker::UnsupportedSchema,
                );
            } else if let Some(entries) = journal.get("entries").and_then(Value::as_array) {
                // Terminal status can still retain file effects. Only an owning
                // converter can attest settlement; never replay old journals.
                for (index, record) in entries.iter().enumerate() {
                    self.retain(
                        &format!("profile/state/operation-journals.json#/entries/{index}"),
                        Some(record.clone()),
                        ImportBlocker::UnsettledOperation,
                    );
                }
            } else {
                self.retain(
                    "profile/state/operation-journals.json",
                    Some(journal),
                    ImportBlocker::UnsupportedSchema,
                );
            }
        }
        let paths: Vec<_> = self
            .files
            .iter()
            .map(|file| file.manifest.relative.clone())
            .collect();
        for path in paths {
            let blocker = if path.starts_with("instances/") {
                let leaf = path
                    .rsplit('/')
                    .next()
                    .unwrap_or_default()
                    .to_ascii_lowercase();
                if leaf.starts_with(".axial-lock") || path.contains("/.axial-performance/") {
                    Some(ImportBlocker::ManagedStateRequiresConversion)
                } else if leaf.starts_with(".axial") || leaf == "axial.content.json" {
                    Some(ImportBlocker::ContentProvenanceRequiresConversion)
                } else {
                    None
                }
            } else if path.starts_with("profile/performance/") {
                Some(ImportBlocker::ManagedStateRequiresConversion)
            } else if path.starts_with("profile/benchmarks/") {
                Some(ImportBlocker::RetainedHistoryRequiresConversion)
            } else if path.starts_with("profile/skins/") {
                Some(ImportBlocker::SavedSkinsRequireConversion)
            } else if matches!(
                path.as_str(),
                "profile/config.json"
                    | "profile/accounts.json"
                    | "profile/instances.json"
                    | "profile/state/operation-journals.json"
                    | REJECTION_STREAK_RECORD
            ) {
                None
            } else {
                Some(ImportBlocker::UnknownRetainedRecord)
            };
            if let Some(blocker) = blocker {
                self.retain(&path, self.records.get(&path).cloned(), blocker);
            }
        }
        // Apply again after parsing: malformed records can precede registry rows.
        for obligation in &self.obligations {
            let unknown_target = obligation.instance_ids.iter().any(|id| {
                !self
                    .instances
                    .iter()
                    .any(|instance| &instance.legacy_id == id)
            });
            for instance in &mut self.instances {
                if obligation.instance_ids.is_empty()
                    || unknown_target
                    || obligation.instance_ids.contains(&instance.legacy_id)
                {
                    push_unique(&mut instance.blockers, obligation.blocker.clone());
                }
            }
        }
    }

    fn retain(&mut self, source: &str, original: Option<Value>, blocker: ImportBlocker) {
        let mut ids = BTreeSet::new();
        if let Some(original) = &original {
            collect_instance_ids(original, &mut ids);
        }
        if let Some(id) = source
            .strip_prefix("instances/")
            .and_then(|v| v.split('/').next())
        {
            ids.insert(id.into());
        }
        if source.starts_with("profile/instances.json#/pending_deletions/") {
            if let Some(id) = original
                .as_ref()
                .and_then(|value| text(value, "id"))
                .filter(|id| legacy_id(id))
            {
                ids.insert(id.into());
            }
        }
        let instance_ids = ids.into_iter().collect();
        self.block(blocker.clone());
        if !self
            .obligations
            .iter()
            .any(|record| record.source_record == source && record.blocker == blocker)
        {
            self.obligations.push(RetainedObligation {
                source_record: source.into(),
                instance_ids,
                original,
                blocker,
            });
        }
    }
    fn block(&mut self, blocker: ImportBlocker) {
        push_unique(&mut self.preview.blockers, blocker);
    }

    fn finalize(&mut self) -> ImportResult<()> {
        self.files
            .sort_by(|a, b| a.manifest.relative.cmp(&b.manifest.relative));
        self.directories.sort_by(|a, b| a.0.cmp(&b.0));
        self.obligations
            .sort_by(|a, b| (&a.source_record, &a.blocker).cmp(&(&b.source_record, &b.blocker)));
        self.preview.blockers.sort();
        for instance in &mut self.instances {
            instance.blockers.sort();
        }
        let mut hash = Sha256::new();
        for file in &self.files {
            hash.update(serde_json::to_vec(&file.manifest).map_err(|_| ImportError::InvalidData)?);
            hash.update([0]);
        }
        for (relative, _, _) in &self.directories {
            hash.update(relative.as_bytes());
            hash.update([0]);
        }
        hash.update(serde_json::to_vec(&self.obligations).map_err(|_| ImportError::InvalidData)?);
        hash.update(
            serde_json::to_vec(&self.preview.blockers).map_err(|_| ImportError::InvalidData)?,
        );
        self.preview.fingerprint = hex::encode(hash.finalize());
        self.preview.metadata_import_id = self.metadata_import_id()?;
        self.preview.metadata_import_available = self.metadata_import_available();
        self.preview.skin_import_id = self.skin_import_id()?;
        self.preview.skin_import_available = self.skin_import_available();
        self.preview.rules_import_id = self.rules_import_id()?;
        self.preview.rules_import_available = self.rules_import_available();
        self.refresh_instance_preview();
        self.preview.file_count = self.files.len();
        self.preview.retained_obligation_count = self.obligations.len();
        self.preview.retained_records = self
            .obligations
            .iter()
            .map(|record| {
                Ok(RetainedRecordPreview {
                    record_id: hex::encode(Sha256::digest(
                        serde_json::to_vec(record).map_err(|_| ImportError::InvalidData)?,
                    )),
                    instance_ids: record.instance_ids.clone(),
                    blocker: record.blocker.clone(),
                })
            })
            .collect::<ImportResult<_>>()?;
        self.check_cancelled()?;
        self.revalidate()
    }

    fn refresh_instance_preview(&mut self) {
        self.preview.instances = self.instance_previews(None);
    }

    pub(super) fn preview_with_rules(
        &self,
        rules: Option<&crate::performance::rules::CompletedRulesImport>,
    ) -> ImportPreview {
        let mut preview = self.preview();
        // Stored display rows may have been enriched before native admission.
        // A missing current fact must withdraw that eligibility, not reuse it.
        preview.instances = self.instance_previews(rules);
        preview
    }

    fn instance_previews(
        &self,
        rules: Option<&crate::performance::rules::CompletedRulesImport>,
    ) -> Vec<InstancePreview> {
        let (history, payloads) = match rules {
            Some(rules) => (
                super::history::prepare_history_with_rules(self, Some(rules)),
                super::prepare::prepare_payloads_with_rules(self, Some(rules)),
            ),
            None => (
                super::history::prepare_history(self),
                super::prepare::prepare_payloads(self),
            ),
        };
        self.instances
            .iter()
            .map(|item| {
                let converted = history.as_ref().ok().and_then(|history| {
                    self.convert_instance_import(&item.legacy_id, history, payloads.as_ref().ok()?)
                        .ok()
                });
                InstancePreview {
                    legacy_id: item.legacy_id.clone(),
                    name: bounded_label(text(&item.original, "name"), "Unavailable instance"),
                    ordinary_import_available: converted.is_some(),
                    loader_key: bounded_label(
                        converted
                            .as_ref()
                            .map(|input| input.loader_key())
                            .or_else(|| text(&item.original, "loader_key")),
                        "",
                    ),
                    blockers: item.blockers.clone(),
                }
            })
            .collect()
    }
}

fn collect_instance_ids(value: &Value, ids: &mut BTreeSet<String>) {
    match value {
        Value::Object(fields) => {
            if let Some(id) = fields
                .get("instance_id")
                .and_then(Value::as_str)
                .filter(|id| legacy_id(id))
            {
                ids.insert(id.into());
            }
            if fields.get("kind").and_then(Value::as_str) == Some("Instance") {
                if let Some(id) = fields
                    .get("id")
                    .and_then(Value::as_str)
                    .filter(|id| legacy_id(id))
                {
                    ids.insert(id.into());
                }
            }
            for value in fields.values() {
                collect_instance_ids(value, ids);
            }
        }
        Value::Array(values) => {
            for value in values {
                collect_instance_ids(value, ids);
            }
        }
        _ => {}
    }
}
fn push_unique(values: &mut Vec<ImportBlocker>, value: ImportBlocker) {
    if !values.contains(&value) {
        values.push(value);
    }
}
fn name(value: &str) -> ImportResult<LeafName> {
    LeafName::new(value).map_err(|_| ImportError::InvalidData)
}
fn text<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}
fn bounded_label(value: Option<&str>, fallback: &str) -> String {
    value
        .filter(|value| value.len() <= 1024 && !value.chars().any(char::is_control))
        .unwrap_or(fallback)
        .into()
}
fn optional_directory(parent: &Directory, leaf: &str) -> ImportResult<Option<Directory>> {
    match parent.open_directory(&name(leaf)?) {
        Ok(directory) => Ok(Some(directory)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}
