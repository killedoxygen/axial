//! Registered-instance resource commands and retained native effect ownership.

use super::{mods, screenshots, worlds};
use crate::{
    content::{install::ContentMutations, provenance::ContentManifest},
    files::{PortableName, ScopedDirectory},
    instances::{directory::InstanceDirectories, model::InstanceId},
    tasks::{TaskHandle, TaskOwner},
};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    io,
    sync::{Arc, Mutex},
};
use ts_rs::TS;

#[derive(Debug, thiserror::Error)]
pub enum ResourceError {
    #[error("invalid resource name or file type")]
    InvalidName,
    #[error("resource not found")]
    NotFound,
    #[error("resource already exists or changed; refresh and try again")]
    Conflict,
    #[error("this instance is in use or unavailable")]
    Busy,
    #[error("resource exceeds safe filesystem limits")]
    Limit,
    #[error("managed mods must be removed through content operations")]
    Managed,
    #[error("resource files could not be read or updated")]
    Files,
    #[error("resource operation requires settlement before the instance can be used")]
    Pending,
}

impl ResourceError {
    pub fn status_code(&self) -> u16 {
        match self {
            Self::InvalidName => 400,
            Self::NotFound => 404,
            Self::Conflict | Self::Busy | Self::Managed | Self::Pending => 409,
            Self::Limit => 413,
            Self::Files => 500,
        }
    }
}

#[derive(Clone, Debug, Serialize, TS)]
pub struct InstanceLogInfo {
    pub name: String,
    pub size: u64,
    pub modified_at: String,
}

#[derive(Debug, Serialize, TS)]
pub struct InstanceLogTailResponse {
    pub name: String,
    pub size: u64,
    pub truncated: bool,
    pub text: String,
}

#[derive(Debug, Serialize, TS)]
pub struct InstanceResourcesResponse {
    pub worlds: Vec<worlds::InstanceWorldInfo>,
    pub mods: Vec<mods::InstanceModInfo>,
    pub screenshots: Vec<screenshots::InstanceScreenshotInfo>,
    pub logs: Vec<InstanceLogInfo>,
    pub worlds_count: usize,
    pub mods_count: usize,
    pub screenshots_count: usize,
    pub logs_count: usize,
}

#[derive(Debug, Serialize)]
pub struct ResourceCommand {
    pub status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backup: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub location: Option<String>,
}

impl ResourceCommand {
    fn ok(name: Option<String>) -> Self {
        Self {
            status: "ok",
            name,
            enabled: None,
            backup: None,
            location: None,
        }
    }
}

#[derive(Clone)]
pub struct ResourceService {
    directories: InstanceDirectories,
    content: ContentMutations,
    tasks: TaskOwner,
    retained: Arc<Mutex<Vec<Box<dyn Send>>>>,
}

impl ResourceService {
    pub fn new(
        directories: InstanceDirectories,
        content: ContentMutations,
        tasks: TaskOwner,
    ) -> Self {
        Self {
            directories,
            content,
            tasks,
            retained: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub fn has_unsettled_effects(&self) -> bool {
        !self
            .retained
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .is_empty()
    }

    fn retain(&self, obligation: impl Send + 'static) -> ResourceError {
        self.retained
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .push(Box::new(obligation));
        ResourceError::Pending
    }

    pub fn resources(&self, id: &InstanceId) -> Result<InstanceResourcesResponse, ResourceError> {
        let instance = self
            .directories
            .admit_read(id)
            .map_err(|_| ResourceError::Busy)?;
        let game = instance.game_directory();
        let worlds = worlds::list_worlds(game).map_err(|_| ResourceError::Files)?;
        let mods = mods::list_mods(game).map_err(|_| ResourceError::Files)?;
        let screenshots = screenshots::list_screenshots(game).map_err(|_| ResourceError::Files)?;
        let logs = list_logs(game)?;
        instance
            .validate_current()
            .map_err(|_| ResourceError::Busy)?;
        Ok(InstanceResourcesResponse {
            worlds_count: worlds.len(),
            mods_count: mods.len(),
            screenshots_count: screenshots.len(),
            logs_count: logs.len(),
            worlds,
            mods,
            screenshots,
            logs,
        })
    }

    pub fn screenshot(
        &self,
        id: &InstanceId,
        name: &str,
    ) -> Result<screenshots::ScreenshotMedia, ResourceError> {
        let instance = self
            .directories
            .admit_read(id)
            .map_err(|_| ResourceError::Busy)?;
        let media =
            screenshots::screenshot_media(instance.game_directory(), name).map_err(|error| {
                match error {
                    screenshots::ScreenshotError::InvalidName => ResourceError::InvalidName,
                    screenshots::ScreenshotError::NotFound => ResourceError::NotFound,
                    screenshots::ScreenshotError::TooLarge => ResourceError::Limit,
                    _ => ResourceError::Files,
                }
            })?;
        instance
            .validate_current()
            .map_err(|_| ResourceError::Busy)?;
        Ok(media)
    }

    pub fn world_icon(
        &self,
        id: &InstanceId,
        name: &str,
    ) -> Result<worlds::WorldIcon, ResourceError> {
        let instance = self
            .directories
            .admit_read(id)
            .map_err(|_| ResourceError::Busy)?;
        let icon = worlds::world_icon(instance.game_directory(), name)
            .map_err(|_| ResourceError::NotFound)?;
        instance
            .validate_current()
            .map_err(|_| ResourceError::Busy)?;
        Ok(icon)
    }

    pub fn log(
        &self,
        id: &InstanceId,
        name: &str,
    ) -> Result<InstanceLogTailResponse, ResourceError> {
        let name = log_name(name)?;
        let instance = self
            .directories
            .admit_read(id)
            .map_err(|_| ResourceError::Busy)?;
        let logs = child(instance.game_directory(), "logs")?;
        let file = logs.open_file(&name).map_err(read_error)?;
        let revision = file.revision().map_err(read_error)?;
        let size = revision.size();
        const LIMIT: u64 = 128 * 1024;
        let length = size.min(LIMIT) as usize;
        let bytes = file
            .capability()
            .read_range_bounded(&revision, size.saturating_sub(LIMIT), length)
            .map_err(read_error)?;
        let truncated = size > LIMIT;
        let redactor = crate::launch::logs::Redactor::new(Vec::new());
        let text = String::from_utf8_lossy(&bytes)
            .lines()
            .map(|line| redactor.redact_line(line))
            .collect::<Vec<_>>()
            .join("\n");
        instance
            .validate_current()
            .map_err(|_| ResourceError::Busy)?;
        Ok(InstanceLogTailResponse {
            name: name.as_str().into(),
            size,
            truncated,
            text,
        })
    }

    pub fn rename_screenshot(
        &self,
        id: &InstanceId,
        from: &str,
        to: &str,
    ) -> Result<TaskHandle<Result<ResourceCommand, ResourceError>>, ResourceError> {
        let (from, to) =
            screenshots::validate_rename(from, to).map_err(|_| ResourceError::InvalidName)?;
        self.move_file(id, "screenshots", from, to, None)
    }

    pub fn delete_screenshot(
        &self,
        id: &InstanceId,
        name: &str,
    ) -> Result<TaskHandle<Result<ResourceCommand, ResourceError>>, ResourceError> {
        let name = screenshots::screenshot_name(name).map_err(|_| ResourceError::InvalidName)?;
        self.delete_file(id, "screenshots", name)
    }

    pub fn set_mod_enabled(
        &self,
        id: &InstanceId,
        name: &str,
        enabled: bool,
    ) -> Result<TaskHandle<Result<ResourceCommand, ResourceError>>, ResourceError> {
        mods::validate_mod_filename(name).map_err(|_| ResourceError::InvalidName)?;
        let manifest = self
            .content
            .installed(id)
            .map_err(|_| ResourceError::Busy)?;
        let key = crate::files::portable::managed_content_name_key(&portable(name)?);
        if let Some(entry) = manifest.entries().iter().find(|entry| {
            entry.kind() == crate::content::model::ContentKind::Mod
                && entry
                    .managed_filename()
                    .is_some_and(|filename| filename.key() == key)
        }) {
            let work = self
                .content
                .set_enabled(id, entry.canonical_id(), enabled)
                .map_err(|_| ResourceError::Conflict)?;
            let filename = entry.managed_filename().ok_or(ResourceError::Managed)?;
            let name = if enabled {
                filename.as_str()
            } else {
                filename.disabled().as_str()
            }
            .to_owned();
            return self
                .tasks
                .try_spawn((), move |_cancel| async move {
                    work.join()
                        .await
                        .map_err(|_| ResourceError::Pending)?
                        .map_err(|_| ResourceError::Pending)?;
                    let mut result = ResourceCommand::ok(Some(name));
                    result.enabled = Some(enabled);
                    Ok(result)
                })
                .map_err(|_| ResourceError::Busy);
        }
        let from = portable(name)?;
        let current_enabled = !from.key().as_str().ends_with(".disabled");
        let to = if current_enabled == enabled {
            from.clone()
        } else if enabled {
            portable(&name[..name.len() - ".disabled".len()])?
        } else {
            from.with_suffix(".disabled")
                .map_err(|_| ResourceError::InvalidName)?
        };
        self.move_file(id, "mods", from, to, Some(enabled))
    }

    pub fn delete_mod(
        &self,
        id: &InstanceId,
        name: &str,
    ) -> Result<TaskHandle<Result<ResourceCommand, ResourceError>>, ResourceError> {
        mods::validate_mod_filename(name).map_err(|_| ResourceError::InvalidName)?;
        self.delete_file(id, "mods", portable(name)?)
    }

    fn move_file(
        &self,
        id: &InstanceId,
        folder: &'static str,
        from: PortableName,
        to: PortableName,
        enabled: Option<bool>,
    ) -> Result<TaskHandle<Result<ResourceCommand, ResourceError>>, ResourceError> {
        let instance = self
            .directories
            .admit(id)
            .map_err(|_| ResourceError::Busy)?;
        let owner = self.clone();
        self.tasks
            .try_spawn(instance.clone(), move |cancel| async move {
                if cancel.is_cancelled() {
                    return Err(ResourceError::Busy);
                }
                let parent = child(instance.game_directory(), folder)?;
                if folder == "mods" {
                    owner
                        .content
                        .protect_performance_mod(&instance, from.as_str())
                        .await
                        .map_err(|_| ResourceError::Managed)?;
                    refuse_managed(instance.game_directory(), &from)?;
                }
                let file = parent.open_file(&from).map_err(read_error)?;
                if from != to {
                    let outcome = file.move_no_replace(&parent, &to);
                    if !matches!(outcome.value(), axial_fs::FileMoveOutcome::Applied(_)) {
                        if let axial_fs::FileMoveOutcome::NoEffect { error, .. } = outcome.value() {
                            return Err(read_error(io::Error::new(
                                error.kind(),
                                "resource move refused",
                            )));
                        }
                        return Err(owner.retain((instance, outcome)));
                    }
                }
                let mut result = ResourceCommand::ok(Some(to.as_str().into()));
                result.enabled = enabled;
                Ok(result)
            })
            .map_err(|_| ResourceError::Busy)
    }

    fn delete_file(
        &self,
        id: &InstanceId,
        folder: &'static str,
        name: PortableName,
    ) -> Result<TaskHandle<Result<ResourceCommand, ResourceError>>, ResourceError> {
        let instance = self
            .directories
            .admit(id)
            .map_err(|_| ResourceError::Busy)?;
        let owner = self.clone();
        self.tasks
            .try_spawn(instance.clone(), move |cancel| async move {
                if cancel.is_cancelled() {
                    return Err(ResourceError::Busy);
                }
                if folder == "mods" {
                    owner
                        .content
                        .protect_performance_mod(&instance, name.as_str())
                        .await
                        .map_err(|_| ResourceError::Managed)?;
                    refuse_managed(instance.game_directory(), &name)?;
                }
                let parent = child(instance.game_directory(), folder)?;
                let file = parent.open_file(&name).map_err(read_error)?;
                let revision = file.revision().map_err(read_error)?;
                let mut reader = file
                    .reader(if folder == "mods" {
                        512 * 1024 * 1024
                    } else {
                        screenshots::SCREENSHOT_FILE_MAX_BYTES
                    })
                    .map_err(read_error)?;
                let mut digest = Sha256::new();
                let mut buffer = [0_u8; 64 * 1024];
                loop {
                    let read = io::Read::read(&mut reader, &mut buffer).map_err(read_error)?;
                    if read == 0 {
                        break;
                    }
                    digest.update(&buffer[..read]);
                }
                reader.finish().map_err(read_error)?;
                file.validate_revision(&revision).map_err(read_error)?;
                let (outcome, pin) = file
                    .park(
                        axial_fs::ExpectedFileContent::new(revision, digest.finalize().into()),
                        &parent,
                    )
                    .into_parts();
                match outcome {
                    axial_fs::FileParkOutcome::Parked(parked) => match parked.remove() {
                        axial_fs::FileRemovalOutcome::Removed => Ok(ResourceCommand::ok(None)),
                        outcome => Err(owner.retain((instance, pin, outcome))),
                    },
                    axial_fs::FileParkOutcome::NoEffect { .. }
                    | axial_fs::FileParkOutcome::Preserved { .. } => Err(ResourceError::Conflict),
                    outcome => Err(owner.retain((instance, pin, outcome))),
                }
            })
            .map_err(|_| ResourceError::Busy)
    }

    pub fn rename_world(
        &self,
        id: &InstanceId,
        from: &str,
        to: &str,
    ) -> Result<TaskHandle<Result<ResourceCommand, ResourceError>>, ResourceError> {
        let from = worlds::world_name(from).map_err(|_| ResourceError::InvalidName)?;
        let to = worlds::world_name(to).map_err(|_| ResourceError::InvalidName)?;
        let instance = self
            .directories
            .admit(id)
            .map_err(|_| ResourceError::Busy)?;
        let owner = self.clone();
        self.tasks
            .try_spawn(instance.clone(), move |cancel| async move {
                if cancel.is_cancelled() {
                    return Err(ResourceError::Busy);
                }
                let saves = child(instance.game_directory(), "saves")?;
                let source = saves.open_directory(&from).map_err(read_error)?;
                if from != to {
                    let outcome = source.move_no_replace(&saves, &to);
                    match outcome.value() {
                        axial_fs::DirectoryMoveOutcome::Applied(_) => (),
                        axial_fs::DirectoryMoveOutcome::NoEffect { error, .. } => {
                            return Err(read_error(io::Error::new(
                                error.kind(),
                                "resource move refused",
                            )));
                        }
                        _ => return Err(owner.retain((instance, outcome))),
                    }
                }
                Ok(ResourceCommand::ok(Some(to.as_str().into())))
            })
            .map_err(|_| ResourceError::Busy)
    }

    pub fn delete_world(
        &self,
        id: &InstanceId,
        name: &str,
    ) -> Result<TaskHandle<Result<ResourceCommand, ResourceError>>, ResourceError> {
        let name = worlds::world_name(name).map_err(|_| ResourceError::InvalidName)?;
        let instance = self
            .directories
            .admit(id)
            .map_err(|_| ResourceError::Busy)?;
        let owner = self.clone();
        self.tasks
            .try_spawn(instance.clone(), move |cancel| async move {
                if cancel.is_cancelled() {
                    return Err(ResourceError::Busy);
                }
                let saves = child(instance.game_directory(), "saves")?;
                let source = saves.open_directory(&name).map_err(read_error)?;
                let (outcome, pin) = source.park().into_parts();
                match outcome {
                    axial_fs::DirectoryParkOutcome::Parked(parked) => match parked.remove_tree() {
                        axial_fs::DirectoryTreeRemovalOutcome::Removed => {
                            Ok(ResourceCommand::ok(None))
                        }
                        outcome => Err(owner.retain((instance, pin, outcome))),
                    },
                    axial_fs::DirectoryParkOutcome::NoEffect { .. } => Err(ResourceError::Conflict),
                    outcome => Err(owner.retain((instance, pin, outcome))),
                }
            })
            .map_err(|_| ResourceError::Busy)
    }

    pub fn backup_world(
        &self,
        id: &InstanceId,
        name: &str,
    ) -> Result<TaskHandle<Result<ResourceCommand, ResourceError>>, ResourceError> {
        use axial_minecraft::managed_path::{
            ManagedTreeCopyLimits, ManagedTreeCopyOutcome, ManagedTreeRoot,
        };
        let name = worlds::world_name(name).map_err(|_| ResourceError::InvalidName)?;
        let instance = self
            .directories
            .admit(id)
            .map_err(|_| ResourceError::Busy)?;
        let owner = self.clone();
        self.tasks
            .try_spawn(instance.clone(), move |cancel| async move {
                if cancel.is_cancelled() {
                    return Err(ResourceError::Busy);
                }
                let native = instance.game_directory().capability();
                let effects = native.create_effect_owner().map_err(read_error)?;
                let root = match ManagedTreeRoot::from_directory(native.clone(), effects.clone()) {
                    Ok(root) => root,
                    Err(error) if !effects.has_pending() => return Err(read_error(error)),
                    Err(_) => return Err(owner.retain((instance, effects))),
                };
                let operation = root.try_acquire().map_err(read_error)?;
                let result = (|| {
                    let game = operation.directory().map_err(read_error)?;
                    let source = game
                        .open_child("saves")
                        .map_err(read_error)?
                        .ok_or(ResourceError::NotFound)?
                        .open_child(name.as_str())
                        .map_err(read_error)?
                        .ok_or(ResourceError::NotFound)?;
                    let destination = game
                        .open_or_create_child("backups")
                        .map_err(read_error)?
                        .open_or_create_child("worlds")
                        .map_err(read_error)?;
                    let timestamp = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
                    let final_names = (1..=worlds::BACKUP_NAME_ATTEMPTS)
                        .map(|attempt| {
                            axial_minecraft::portable_path::PortableFileName::new_exact(
                                worlds::backup_name(&name, &timestamp, attempt).as_str(),
                            )
                            .expect("portable generated name")
                        })
                        .collect::<Vec<_>>();
                    let stage = axial_minecraft::portable_path::PortableFileName::new_exact(
                        &format!("stage-{}", uuid::Uuid::new_v4()),
                    )
                    .map_err(|_| ResourceError::InvalidName)?;
                    match destination.copy_tree_no_replace(
                        &source,
                        &final_names,
                        &[stage],
                        ManagedTreeCopyLimits {
                            max_depth: worlds::WORLD_BACKUP_MAX_DEPTH,
                            max_entries: worlds::WORLD_BACKUP_MAX_ENTRIES,
                            max_bytes: worlds::WORLD_BACKUP_MAX_BYTES,
                        },
                    ) {
                        ManagedTreeCopyOutcome::Applied(name) => {
                            let mut result = ResourceCommand::ok(None);
                            result.backup = Some(name.as_str().into());
                            result.location = Some(format!("backups/worlds/{}", name.as_str()));
                            Ok(result)
                        }
                        ManagedTreeCopyOutcome::RefusedBeforeMove(_) => Err(ResourceError::Files),
                        _ => Err(ResourceError::Pending),
                    }
                })();
                if effects.has_pending() || matches!(result, Err(ResourceError::Pending)) {
                    return Err(owner.retain((instance, root, operation, effects)));
                }
                result
            })
            .map_err(|_| ResourceError::Busy)
    }
}

fn portable(name: &str) -> Result<PortableName, ResourceError> {
    PortableName::new_exact(name).map_err(|_| ResourceError::InvalidName)
}
fn child(game: &ScopedDirectory, name: &str) -> Result<ScopedDirectory, ResourceError> {
    game.open_directory(&portable(name)?).map_err(read_error)
}
fn read_error(error: io::Error) -> ResourceError {
    match error.kind() {
        io::ErrorKind::NotFound => ResourceError::NotFound,
        io::ErrorKind::AlreadyExists | io::ErrorKind::InvalidData => ResourceError::Conflict,
        _ => ResourceError::Files,
    }
}
fn refuse_managed(game: &ScopedDirectory, name: &PortableName) -> Result<(), ResourceError> {
    let path = crate::files::ScopedPath::new_exact(crate::content::provenance::MANIFEST_FILE)
        .expect("fixed name");
    let bytes =
        match game.read_bounded(&path, crate::content::provenance::MAX_MANIFEST_BYTES as u64) {
            Ok(bytes) => Some(bytes),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(_) => return Err(ResourceError::Files),
        };
    let manifest =
        ContentManifest::decode_managed(bytes.as_deref()).map_err(|_| ResourceError::Conflict)?;
    let key = crate::files::portable::managed_content_name_key(name);
    if manifest.entries().iter().any(|entry| {
        entry.kind() == crate::content::model::ContentKind::Mod
            && entry
                .managed_filename()
                .is_some_and(|name| name.key() == key)
    }) {
        return Err(ResourceError::Managed);
    }
    Ok(())
}
fn log_name(name: &str) -> Result<PortableName, ResourceError> {
    let name = portable(name)?;
    if !name.key().as_str().ends_with(".log") && !name.key().as_str().ends_with(".log.gz") {
        return Err(ResourceError::InvalidName);
    }
    Ok(name)
}
fn list_logs(game: &ScopedDirectory) -> Result<Vec<InstanceLogInfo>, ResourceError> {
    let logs = match child(game, "logs") {
        Ok(logs) => logs,
        Err(ResourceError::NotFound) => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let listing = logs.entries(50_000).map_err(read_error)?;
    if listing.state() != axial_fs::DirectoryListingState::Complete {
        return Err(ResourceError::Limit);
    }
    let mut result = Vec::new();
    for entry in listing.entries() {
        if matches!(
            entry.kind(),
            axial_fs::EntryKind::Link | axial_fs::EntryKind::Other
        ) {
            return Err(ResourceError::Conflict);
        }
        if entry.kind() != axial_fs::EntryKind::File {
            continue;
        }
        let Some(name) = entry.utf8_name().and_then(|name| log_name(name).ok()) else {
            continue;
        };
        let revision = logs
            .open_file(&name)
            .map_err(read_error)?
            .revision()
            .map_err(read_error)?;
        result.push(InstanceLogInfo {
            name: name.as_str().into(),
            size: revision.size(),
            modified_at: revision
                .modified_at_ns()
                .ok()
                .map(super::timestamp)
                .unwrap_or_default(),
        });
    }
    result.sort_by(|a, b| {
        b.modified_at
            .cmp(&a.modified_at)
            .then_with(|| a.name.cmp(&b.name))
    });
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        instances::{
            create::{CreateInstanceRequest, CreateTarget, InstanceService},
            directory::Registry,
        },
        library::{LibraryLifecycle, LibraryOpenOutcome},
        network::{ClientConfig, ProviderClient},
        storage::MetadataStore,
        tasks::Exclusions,
    };

    async fn fixture() -> (tempfile::TempDir, ResourceService, InstanceId) {
        let root = tempfile::tempdir().unwrap();
        let library = match LibraryLifecycle::open(&root.path().canonicalize().unwrap()) {
            LibraryOpenOutcome::Ready(library) => library,
            other => panic!("fixture root: {other:?}"),
        };
        let storage = Arc::new(MetadataStore::in_memory().unwrap());
        storage
            .migrate(&[
                crate::instances::directory::MIGRATION,
                crate::instances::create::MIGRATION,
                crate::instances::create::DUPLICATE_WITNESS_MIGRATION,
                crate::instances::delete::MIGRATION,
                crate::content::install::MIGRATION,
                crate::performance::mutation::MIGRATION,
            ])
            .unwrap();
        let directories =
            InstanceDirectories::new(Registry::new(storage), library, Exclusions::new());
        let tasks = TaskOwner::new(16).unwrap();
        let instances = InstanceService::new(directories.clone(), tasks.clone());
        let instance = instances
            .create(
                CreateInstanceRequest {
                    name: "Resources".into(),
                    selection_id: "vanilla|1.21.4".into(),
                    ..Default::default()
                },
                CreateTarget {
                    selection_id: "vanilla|1.21.4".into(),
                    version_id: "1.21.4".into(),
                    minecraft_version: "1.21.4".into(),
                    loader_key: "vanilla".into(),
                },
            )
            .unwrap()
            .join()
            .await
            .unwrap()
            .unwrap();
        let content = ContentMutations::new(
            directories.clone(),
            ProviderClient::new(ClientConfig::default()).unwrap(),
            tasks.clone(),
        );
        (
            root,
            ResourceService::new(directories, content, tasks),
            instance.id,
        )
    }

    #[tokio::test]
    async fn resource_reads_work_during_launch_while_mutations_remain_exclusive() {
        let (root, service, id) = fixture().await;
        let game = root.path().join("instances").join(id.as_str());
        let image = b"\x89PNG\r\n\x1a\noriginal image";
        std::fs::create_dir(game.join("saves/Playing")).unwrap();
        std::fs::write(game.join("saves/Playing/icon.png"), image).unwrap();
        std::fs::write(game.join("screenshots/shot.png"), image).unwrap();
        std::fs::write(game.join("logs/previous.log"), b"retained log\n").unwrap();
        std::fs::write(game.join("mods/local.jar"), b"local mod").unwrap();
        let launch = service.directories.admit(&id).unwrap();
        let read = service.directories.admit_read(&id).unwrap();
        launch
            .record_successful_launch("2026-09-27T10:00:00.000Z")
            .unwrap();
        read.validate_current().unwrap();
        let resources = service.resources(&id).unwrap();
        assert_eq!(
            (
                resources.worlds_count,
                resources.mods_count,
                resources.screenshots_count,
                resources.logs_count
            ),
            (1, 1, 1, 1)
        );
        assert_eq!(service.screenshot(&id, "shot.png").unwrap().bytes, image);
        assert_eq!(service.world_icon(&id, "Playing").unwrap().bytes, image);
        assert_eq!(
            service.log(&id, "previous.log").unwrap().text,
            "retained log"
        );
        assert!(matches!(
            service.rename_screenshot(&id, "shot.png", "renamed.png"),
            Err(ResourceError::Busy)
        ));
        assert!(matches!(
            service.delete_screenshot(&id, "shot.png"),
            Err(ResourceError::Busy)
        ));
        assert!(matches!(
            service.delete_mod(&id, "local.jar"),
            Err(ResourceError::Busy)
        ));
        assert!(matches!(
            service.rename_world(&id, "Playing", "Renamed"),
            Err(ResourceError::Busy)
        ));
        assert!(matches!(
            service.delete_world(&id, "Playing"),
            Err(ResourceError::Busy)
        ));
        assert!(matches!(
            service.backup_world(&id, "Playing"),
            Err(ResourceError::Busy)
        ));
        assert_eq!(
            std::fs::read(game.join("screenshots/shot.png")).unwrap(),
            image
        );
        assert!(!service.has_unsettled_effects());
    }

    #[tokio::test]
    async fn read_admission_refuses_a_deleted_binding_after_observation() {
        let (root, service, id) = fixture().await;
        let game = root.path().join("instances").join(id.as_str());
        std::fs::write(game.join("screenshots/shot.png"), b"original").unwrap();
        let read = service.directories.admit_read(&id).unwrap();
        let observed = screenshots::screenshot_media(read.game_directory(), "shot.png").unwrap();
        let registry = service.directories.registry();
        let record = registry.get_live(&id).unwrap();
        registry
            .storage()
            .transaction(|tx| registry.mark_deleting(tx, &id, record.revision))
            .unwrap();
        assert!(read.validate_current().is_err());
        assert!(matches!(
            service.screenshot(&id, "shot.png"),
            Err(ResourceError::Busy)
        ));
        assert_eq!(observed.bytes, b"original");
        assert_eq!(
            std::fs::read(game.join("screenshots/shot.png")).unwrap(),
            b"original"
        );
    }

    #[tokio::test]
    async fn read_admission_never_follows_a_replacement_instance_directory() {
        let (root, service, id) = fixture().await;
        let game = root.path().join("instances").join(id.as_str());
        std::fs::write(game.join("screenshots/shot.png"), b"original").unwrap();
        let read = service.directories.admit_read(&id).unwrap();
        let observed = screenshots::screenshot_media(read.game_directory(), "shot.png").unwrap();
        let preserved = root.path().join("preserved-instance");
        std::fs::rename(&game, &preserved).unwrap();
        std::fs::create_dir_all(game.join("screenshots")).unwrap();
        std::fs::write(game.join("screenshots/shot.png"), b"replacement").unwrap();
        assert!(read.validate_current().is_err());
        assert!(matches!(
            service.screenshot(&id, "shot.png"),
            Err(ResourceError::Busy)
        ));
        assert_eq!(observed.bytes, b"original");
        assert_eq!(
            std::fs::read(preserved.join("screenshots/shot.png")).unwrap(),
            b"original"
        );
        assert_eq!(
            std::fs::read(game.join("screenshots/shot.png")).unwrap(),
            b"replacement"
        );
    }

    #[tokio::test]
    async fn pending_effects_fence_reads_before_and_after_observation() {
        let (_root, service, id) = fixture().await;
        for (insert, remove) in [
            (
                "INSERT INTO content_batches VALUES(?1,'pending','{}')",
                "DELETE FROM content_batches WHERE instance_id=?1",
            ),
            (
                "INSERT INTO performance_operations VALUES(?1,'pending',X'7b7d')",
                "DELETE FROM performance_operations WHERE instance_id=?1",
            ),
            (
                "INSERT INTO instance_setups VALUES(?1,'pending','{}','pending')",
                "DELETE FROM instance_setups WHERE instance_id=?1",
            ),
        ] {
            let read = service.directories.admit_read(&id).unwrap();
            service
                .directories
                .registry()
                .storage()
                .transaction(|tx| -> Result<_, crate::storage::StorageError> {
                    tx.execute(insert, [id.as_str()])?;
                    Ok(())
                })
                .unwrap();
            assert!(read.validate_current().is_err());
            assert!(matches!(service.resources(&id), Err(ResourceError::Busy)));
            service
                .directories
                .registry()
                .storage()
                .transaction(|tx| -> Result<_, crate::storage::StorageError> {
                    tx.execute(remove, [id.as_str()])?;
                    Ok(())
                })
                .unwrap();
            read.validate_current().unwrap();
        }
    }

    #[tokio::test]
    async fn read_pin_survives_library_switch_and_blocks_root_retirement() {
        let (_root, service, id) = fixture().await;
        let read = service.directories.admit_read(&id).unwrap();
        let library = service.directories.library();
        let mut change = library.begin_switch().unwrap();
        change
            .prepare_managed(crate::library::LibraryId::new())
            .unwrap();
        change.commit_after_persistence().unwrap();
        read.validate_current().unwrap();
        assert!(matches!(service.resources(&id), Err(ResourceError::Busy)));
        assert!(!library.collect_retired());
        library.close_admission();
        assert!(matches!(
            library.begin_root_reset(),
            Err(crate::library::LibraryError::RetirementPending)
        ));
        read.validate_current().unwrap();
        drop(read);
        library
            .wait_for_pins(std::time::Duration::from_secs(1))
            .await
            .unwrap();
        assert!(matches!(
            library.revoke_application_root().unwrap(),
            axial_fs::RootRevokeOutcome::Revoked
        ));
    }

    #[tokio::test]
    async fn world_backup_rename_and_delete_preserve_source_bytes_and_unrelated_worlds() {
        let (root, service, id) = fixture().await;
        let game = root.path().join("instances").join(id.as_str());
        std::fs::create_dir_all(game.join("saves/Original/region")).unwrap();
        std::fs::write(game.join("saves/Original/region/r.0.0.mca"), b"world bytes").unwrap();
        std::fs::create_dir(game.join("saves/Other")).unwrap();
        std::fs::write(game.join("saves/Other/level.dat"), b"keep").unwrap();
        let backup = service
            .backup_world(&id, "Original")
            .unwrap()
            .join()
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            std::fs::read(game.join(backup.location.unwrap()).join("region/r.0.0.mca")).unwrap(),
            b"world bytes"
        );
        assert_eq!(
            std::fs::read(game.join("saves/Original/region/r.0.0.mca")).unwrap(),
            b"world bytes"
        );
        service
            .rename_world(&id, "Original", "Renamed")
            .unwrap()
            .join()
            .await
            .unwrap()
            .unwrap();
        service
            .delete_world(&id, "Renamed")
            .unwrap()
            .join()
            .await
            .unwrap()
            .unwrap();
        assert!(!game.join("saves/Original").exists());
        assert!(!game.join("saves/Renamed").exists());
        assert_eq!(
            std::fs::read(game.join("saves/Other/level.dat")).unwrap(),
            b"keep"
        );
        assert!(!service.has_unsettled_effects());
    }

    #[tokio::test]
    async fn screenshot_original_bytes_and_real_log_tail_survive_resource_actions() {
        let (root, service, id) = fixture().await;
        let game = root.path().join("instances").join(id.as_str());
        std::fs::write(game.join("screenshots/shot.PNG"), b"original image bytes").unwrap();
        assert_eq!(
            service.screenshot(&id, "shot.PNG").unwrap().bytes,
            b"original image bytes"
        );
        assert!(
            service
                .rename_screenshot(&id, "shot.PNG", "shot.jpeg")
                .is_err()
        );
        service
            .rename_screenshot(&id, "shot.PNG", "renamed.png")
            .unwrap()
            .join()
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            service.screenshot(&id, "renamed.png").unwrap().bytes,
            b"original image bytes"
        );
        let mut log = b"old line\n".repeat(20_000);
        log.extend_from_slice(b"final line\n");
        std::fs::write(game.join("logs/latest.log"), log).unwrap();
        let tail = service.log(&id, "latest.log").unwrap();
        assert!(tail.truncated);
        assert!(tail.text.ends_with("final line"));
        service
            .delete_screenshot(&id, "renamed.png")
            .unwrap()
            .join()
            .await
            .unwrap()
            .unwrap();
        assert!(service.resources(&id).unwrap().screenshots.is_empty());
        assert!(!service.has_unsettled_effects());
    }
}
