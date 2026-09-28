//! Folder windows originate from registered instance authority, never a path
//! supplied by the caller. The host owns only the native process adapter.

use crate::{
    files::{PortableName, ScopedDirectory},
    instances::{
        create::InstanceService,
        directory::RegisteredInstance,
        model::{InstanceError, InstanceId},
    },
    tasks::{CancellationToken, SpawnError, TaskOwner},
};
use std::{
    io,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, oneshot};

const SUBFOLDERS: [&str; 7] = [
    "mods",
    "saves",
    "resourcepacks",
    "shaderpacks",
    "config",
    "screenshots",
    "logs",
];
const MAX_OPENERS: usize = 8;
const POLL_INTERVAL: Duration = Duration::from_millis(50);

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum FolderError {
    #[error("invalid instance folder")]
    InvalidFolder,
    #[error("instance not found")]
    NotFound,
    #[error("this instance is in use or unavailable")]
    Busy,
    #[error("Too many folder windows are still opening. Try again shortly.")]
    Capacity,
    #[error("Application shutdown is in progress. Try opening the folder again.")]
    Closed,
    #[error("Could not prepare the instance folder. Check app data permissions and try again.")]
    Prepare,
    #[error("Could not open the instance folder. Check desktop permissions and try again.")]
    Spawn,
    #[error("The folder operation requires settlement before this instance can be used.")]
    Pending,
}

impl FolderError {
    pub fn status_code(self) -> u16 {
        match self {
            Self::InvalidFolder => 400,
            Self::NotFound => 404,
            Self::Busy | Self::Pending => 409,
            Self::Capacity | Self::Closed => 503,
            Self::Prepare | Self::Spawn => 500,
        }
    }
}

/// An exact prepared directory and its registered instance admission. This has
/// no public constructor or deserializer; a native adapter cannot accept paths.
pub struct AdmittedFolder {
    instance: RegisteredInstance,
    directory: ScopedDirectory,
}

impl AdmittedFolder {
    /// Borrow this capability through process settlement. The returned spelling
    /// is a checked OS projection, not authority to reopen or mutate other files.
    /// Native adapters call this immediately before spawning the opener.
    pub fn checked_path(&self) -> Result<PathBuf, FolderError> {
        self.instance
            .validate_current()
            .map_err(|_| FolderError::Prepare)?;
        self.directory
            .read_projection()
            .map_err(|_| FolderError::Prepare)
    }
}

/// The real I/O seam also keeps tests from launching an external file manager.
pub trait FolderOpener: Send + Sync + 'static {
    fn spawn(&self, folder: &AdmittedFolder) -> Result<Box<dyn FolderProcess>, FolderError>;
}

pub trait FolderProcess: Send + 'static {
    /// `true` means this exact opener process was reaped. It does not describe
    /// the file manager application, which may already have been running.
    fn try_wait(&mut self) -> io::Result<bool>;
    fn terminate(&mut self) -> io::Result<()>;
}

/// Dropping an HTTP waiter does not cancel the accepted opener or its leases.
pub struct FolderOpen {
    ready: oneshot::Receiver<Result<(), FolderError>>,
}

impl FolderOpen {
    pub async fn wait(self) -> Result<(), FolderError> {
        self.ready.await.unwrap_or(Err(FolderError::Pending))
    }
}

#[derive(Clone)]
pub struct FolderService {
    instances: Arc<InstanceService>,
    tasks: TaskOwner,
    opener: Arc<dyn FolderOpener>,
    slots: Arc<Semaphore>,
}

struct NativeOpen {
    _folder: AdmittedFolder,
    process: Box<dyn FolderProcess>,
}

// The task owner keeps this outside the future as well, so a panic cannot drop
// the process capability, instance exclusion, or physical library generation.
struct OpenLifetime {
    instance: RegisteredInstance,
    _slot: OwnedSemaphorePermit,
    native: Mutex<Option<NativeOpen>>,
}

impl FolderService {
    pub fn new(
        instances: Arc<InstanceService>,
        tasks: TaskOwner,
        opener: Arc<dyn FolderOpener>,
    ) -> Self {
        Self {
            instances,
            tasks,
            opener,
            slots: Arc::new(Semaphore::new(MAX_OPENERS)),
        }
    }

    pub fn open(&self, id: &InstanceId, sub: Option<&str>) -> Result<FolderOpen, FolderError> {
        // Match the retained missing-instance result before checking selectors.
        self.instances
            .registry()
            .get_live(id)
            .map_err(instance_error)?;
        let sub = sub
            .map(|sub| {
                SUBFOLDERS
                    .contains(&sub)
                    .then(|| PortableName::new_exact(sub).expect("fixed portable folder"))
                    .ok_or(FolderError::InvalidFolder)
            })
            .transpose()?;
        let slot = self
            .slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| FolderError::Capacity)?;
        let instance = self
            .instances
            .directories()
            .admit(id)
            .map_err(instance_error)?;
        let lifetime = Arc::new(OpenLifetime {
            instance,
            _slot: slot,
            native: Mutex::new(None),
        });
        let instances = self.instances.for_operation(id.clone());
        let opener = self.opener.clone();
        let (ready, waiting) = oneshot::channel();
        self.tasks
            .try_spawn(lifetime.clone(), move |cancel| async move {
                let preparing = lifetime.clone();
                let preparation_cancel = cancel.clone();
                let prepared = tokio::task::spawn_blocking(move || {
                    prepare_and_spawn(&instances, &preparing, sub, opener, &preparation_cancel)
                })
                .await;
                match prepared {
                    Ok(Ok(valid)) => {
                        let _ = ready.send(if valid {
                            Ok(())
                        } else {
                            Err(FolderError::Prepare)
                        });
                        own_process(lifetime, cancel, !valid).await;
                    }
                    Ok(Err(error)) => {
                        let _ = ready.send(Err(error));
                    }
                    Err(_) => {
                        let _ = ready.send(Err(FolderError::Pending));
                        // Preserve the task owner's outer lifetime after an
                        // unobserved blocking effect instead of declaring idle.
                        panic!("folder preparation did not settle");
                    }
                }
            })
            .map_err(|error| match error {
                SpawnError::Closed => FolderError::Closed,
                SpawnError::AtCapacity => FolderError::Capacity,
                _ => FolderError::Prepare,
            })?;
        Ok(FolderOpen { ready: waiting })
    }
}

fn prepare_and_spawn(
    instances: &InstanceService,
    lifetime: &OpenLifetime,
    sub: Option<PortableName>,
    opener: Arc<dyn FolderOpener>,
    cancel: &CancellationToken,
) -> Result<bool, FolderError> {
    if cancel.is_cancelled() {
        return Err(FolderError::Closed);
    }
    lifetime
        .instance
        .validate_current()
        .map_err(instance_error)?;
    let game = lifetime.instance.game_directory();
    let directory = match sub {
        None => game.clone(),
        Some(name) => match game.open_directory(&name) {
            Ok(directory) => directory,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                match instances.fresh_directory(game, &name) {
                    Ok(directory) => directory,
                    Err(InstanceError::SettlementRequired) => {
                        instances.hold_admission(
                            lifetime.instance.record().instance.id.clone(),
                            lifetime.instance.generation().clone(),
                            lifetime.instance.exclusion().clone(),
                        );
                        return Err(FolderError::Pending);
                    }
                    Err(_) => return Err(FolderError::Prepare),
                }
            }
            Err(_) => return Err(FolderError::Prepare),
        },
    };
    let folder = AdmittedFolder {
        instance: lifetime.instance.clone(),
        directory,
    };
    folder.checked_path()?;
    if cancel.is_cancelled() {
        return Err(FolderError::Closed);
    }
    let process = opener.spawn(&folder)?;
    let valid = folder.checked_path().is_ok();
    *lifetime
        .native
        .lock()
        .unwrap_or_else(|poison| poison.into_inner()) = Some(NativeOpen {
        _folder: folder,
        process,
    });
    Ok(valid)
}

async fn own_process(lifetime: Arc<OpenLifetime>, cancel: CancellationToken, mut stopping: bool) {
    loop {
        stopping |= cancel.is_cancelled();
        {
            let mut native = lifetime
                .native
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            let active = native
                .as_mut()
                .expect("accepted folder process is retained");
            if stopping {
                let _ = active.process.terminate();
            }
            match active.process.try_wait() {
                Ok(true) => {
                    native.take();
                    return;
                }
                Ok(false) => {}
                // A failed wait cannot prove the process exited. Keep its exact
                // capability and retry; bounded application shutdown can refuse.
                Err(_) => stopping = true,
            }
        }
        tokio::select! {
            _ = tokio::time::sleep(POLL_INTERVAL) => {},
            _ = cancel.cancelled(), if !stopping => stopping = true,
        }
    }
}

fn instance_error(error: InstanceError) -> FolderError {
    match error {
        InstanceError::NotFound => FolderError::NotFound,
        InstanceError::Busy | InstanceError::LibraryUnavailable => FolderError::Busy,
        InstanceError::SettlementRequired => FolderError::Pending,
        _ => FolderError::Prepare,
    }
}

#[cfg(test)]
#[path = "folders_tests.rs"]
mod tests;
