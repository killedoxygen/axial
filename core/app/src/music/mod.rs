//! Fixed background tracks with lazy, bounded, application-owned cache work.

mod cache;

use crate::{
    library::{ApplicationRootPin, LibraryLifecycle},
    tasks::{CancellationToken, TaskOwner},
};
use axial_minecraft::download::{
    CreateOnlyTransferTarget, ManagedTransferAuthority, RetryPolicy, TransferClient,
    TransferClientConfig, TransferContract, TransferOrigin, start_create_only_transfer,
    transfer_cancellation_channel,
};
use serde::Serialize;
use std::{
    num::NonZeroU64,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::watch;

pub const MUSIC_MAX_BYTES: u64 = 32 * 1024 * 1024;
pub const MUSIC_FILES: [&str; 2] = ["vapor-halo.mp3", "sublunar-hum.mp3"];
const MUSIC_SOURCES: [&str; 2] = [
    "https://github.com/mateoltd/axial/releases/download/music-v2/vapor-halo.mp3",
    "https://github.com/mateoltd/axial/releases/download/music-v2/sublunar-hum.mp3",
];

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct MusicTrackStatus {
    pub cached: bool,
    pub file: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct MusicStatusResponse {
    pub tracks: Vec<MusicTrackStatus>,
    pub count: usize,
}

#[derive(Clone)]
pub struct MusicTrackBytes {
    pub bytes: Arc<[u8]>,
    pub content_type: &'static str,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum MusicError {
    #[error("Background music is temporarily unavailable.")]
    Unavailable,
    #[error("Background music was not found.")]
    NotFound,
    #[error("Could not load background music. Check your connection and try again.")]
    DownloadFailed,
}

type FlightResult = Option<Result<MusicTrackBytes, MusicError>>;

#[derive(Clone)]
pub struct MusicService {
    shared: Arc<Shared>,
}

struct Shared {
    library: LibraryLifecycle,
    tasks: TaskOwner,
    client: TransferClient,
    sources: [reqwest::Url; 2],
    flights: Mutex<[Option<watch::Receiver<FlightResult>>; 2]>,
    directory_creation: Mutex<()>,
    effects: Mutex<Vec<cache::RetainedEffect>>,
}

impl MusicService {
    /// No filesystem mutation or provider request occurs during construction.
    pub fn new(library: LibraryLifecycle, tasks: TaskOwner) -> Result<Self, MusicError> {
        let sources =
            MUSIC_SOURCES.map(|source| reqwest::Url::parse(source).expect("fixed music URL"));
        let origins = [
            "https://github.com/",
            "https://release-assets.githubusercontent.com/",
        ]
        .into_iter()
        .map(|origin| {
            TransferOrigin::from_url(&reqwest::Url::parse(origin).expect("fixed music origin"))
                .expect("fixed HTTPS music origin")
        })
        .collect();
        Self::with_sources(library, tasks, sources, origins)
    }

    fn with_sources(
        library: LibraryLifecycle,
        tasks: TaskOwner,
        sources: [reqwest::Url; 2],
        origins: Vec<TransferOrigin>,
    ) -> Result<Self, MusicError> {
        let config = TransferClientConfig::bounded(
            Duration::from_secs(10),
            Duration::from_secs(120),
            Duration::from_secs(120),
            origins,
        )
        .map_err(|_| MusicError::Unavailable)?;
        let client = TransferClient::build(config).map_err(|_| MusicError::Unavailable)?;
        Ok(Self {
            shared: Arc::new(Shared {
                library,
                tasks,
                client,
                sources,
                flights: Mutex::new([None, None]),
                directory_creation: Mutex::new(()),
                effects: Mutex::new(Vec::new()),
            }),
        })
    }

    pub async fn status(&self) -> Result<MusicStatusResponse, MusicError> {
        let pin = self
            .shared
            .library
            .admit_application_root()
            .map_err(|_| MusicError::Unavailable)?;
        let worker_pin = pin.clone();
        let task = self
            .shared
            .tasks
            .try_spawn(pin, move |cancellation| async move {
                if cancellation.is_cancelled() {
                    return Err(MusicError::Unavailable);
                }
                tokio::task::spawn_blocking(move || cache::status(&worker_pin))
                    .await
                    .map_err(|_| MusicError::Unavailable)
            })
            .map_err(|_| MusicError::Unavailable)?;
        task.join().await.map_err(|_| MusicError::Unavailable)?
    }

    /// The legacy query defaults to the first track and clamps large indices.
    /// Dropping an HTTP waiter leaves the shared, owned operation running.
    pub async fn track(&self, index: Option<usize>) -> Result<MusicTrackBytes, MusicError> {
        let index = index.unwrap_or(0).min(MUSIC_FILES.len() - 1);
        let mut receiver = {
            let mut flights = self
                .shared
                .flights
                .lock()
                .map_err(|_| MusicError::Unavailable)?;
            if self.shared.tasks.status().closing {
                return Err(MusicError::Unavailable);
            }
            if let Some(receiver) = flights[index]
                .as_ref()
                .filter(|receiver| receiver.borrow().is_none())
            {
                receiver.clone()
            } else {
                let pin = self
                    .shared
                    .library
                    .admit_application_root()
                    .map_err(|_| MusicError::Unavailable)?;
                let (completion, receiver) = watch::channel(None);
                let service = self.clone();
                let worker_pin = pin.clone();
                // The task owner retains admission and effects if its future panics.
                let accepted = self
                    .shared
                    .tasks
                    .try_spawn(pin, move |cancellation| async move {
                        let result = service.load_track(worker_pin, index, cancellation).await;
                        completion.send_replace(Some(result));
                    })
                    .map_err(|_| MusicError::Unavailable)?;
                drop(accepted);
                flights[index] = Some(receiver.clone());
                receiver
            }
        };
        loop {
            if let Some(result) = receiver.borrow_and_update().clone() {
                return result;
            }
            receiver
                .changed()
                .await
                .map_err(|_| MusicError::Unavailable)?;
        }
    }

    /// Call on a blocking worker after the shared TaskOwner drains and before
    /// releasing the library root. Unresolved effects retain their exact pin.
    pub fn settle(&self) -> Result<(), MusicError> {
        let mut effects = self
            .shared
            .effects
            .lock()
            .map_err(|_| MusicError::Unavailable)?;
        let remaining = std::mem::take(&mut *effects)
            .into_iter()
            .filter_map(cache::RetainedEffect::reconcile)
            .collect();
        *effects = remaining;
        if effects.is_empty() {
            Ok(())
        } else {
            Err(MusicError::Unavailable)
        }
    }

    pub fn has_unsettled_effects(&self) -> bool {
        self.shared
            .effects
            .lock()
            .map_or(true, |effects| !effects.is_empty())
    }

    async fn load_track(
        &self,
        pin: ApplicationRootPin,
        index: usize,
        cancellation: CancellationToken,
    ) -> Result<MusicTrackBytes, MusicError> {
        if cancellation.is_cancelled() {
            return Err(MusicError::Unavailable);
        }
        let service = self.clone();
        let check_pin = pin.clone();
        let prepared = tokio::task::spawn_blocking(move || {
            service.settle()?;
            if let Some(bytes) = cache::read(&check_pin, index)? {
                return Ok(Prepared::Cached(bytes));
            }
            let directory = cache::prepare_directory(&service, &check_pin)?;
            match directory.admit_transient_destination(cache::track_name(index)) {
                Ok(destination) => Ok(Prepared::Target(CreateOnlyTransferTarget::new(
                    destination,
                    ManagedTransferAuthority::retain(Arc::new(check_pin)),
                ))),
                Err(_) => cache::read(&check_pin, index)?
                    .map(Prepared::Cached)
                    .ok_or(MusicError::DownloadFailed),
            }
        })
        .await
        .expect("music cache preparation was interrupted")?;
        let target = match prepared {
            Prepared::Cached(bytes) => return Ok(track_bytes(bytes)),
            Prepared::Target(target) => target,
        };
        let (cancel_sender, transfer_cancellation) = transfer_cancellation_channel();
        let transfer = start_create_only_transfer(
            self.shared.client.clone(),
            self.shared.sources[index].clone(),
            target,
            TransferContract::unauthenticated_at_most(
                NonZeroU64::new(MUSIC_MAX_BYTES).expect("positive music limit"),
            ),
            RetryPolicy::none(),
            transfer_cancellation,
        );
        let joined = transfer.join();
        tokio::pin!(joined);
        let outcome = tokio::select! {
            biased;
            _ = cancellation.cancelled() => { cancel_sender.cancel(); joined.await },
            outcome = &mut joined => outcome,
        };
        drop(cancel_sender);
        let service = self.clone();
        tokio::task::spawn_blocking(move || {
            cache::finish(&service, &pin, index, outcome, &cancellation).map(track_bytes)
        })
        .await
        .expect("music cache settlement was interrupted")
    }

    fn retain_effect(&self, pin: ApplicationRootPin, effect: cache::Effect) {
        self.shared
            .effects
            .lock()
            .expect("music effect lock poisoned")
            .push(cache::RetainedEffect { pin, effect });
    }
}

enum Prepared {
    Cached(Vec<u8>),
    Target(CreateOnlyTransferTarget),
}

fn track_bytes(bytes: Vec<u8>) -> MusicTrackBytes {
    MusicTrackBytes {
        bytes: bytes.into(),
        content_type: "audio/mpeg",
    }
}

#[cfg(test)]
mod tests;
