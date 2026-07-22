//! Application-owned music cache workflow.

use crate::state::{
    AppState, MUSIC_MAX_BYTES, MUSIC_TRACKS, MusicCacheOwner, MusicFlightClaim,
    MusicFlightCompletion, MusicTrackId, ProducerLease, RequestProducerHandoff,
};
use axial_fs::LeafName;
use axial_minecraft::download::{
    CreateOnlyTransferTarget, RetryPolicy, TransferCleanupResolution, TransferContract,
    TransferOutcome, TransferPublicationOutcome, VerifiedCreateOnly, VerifiedTransferDiscardOutcome,
    start_create_only_transfer, transfer_cancellation_channel,
};
use serde::Serialize;
use std::future::Future;
use std::num::NonZeroU64;

const MUSIC_DOWNLOAD_FAILURE_COPY: &str =
    "Could not load background music. Check your connection and try again.";
const MUSIC_CONTENT_TYPE: &str = "audio/mpeg";

#[derive(Debug, Serialize)]
pub struct MusicTrackStatus {
    pub cached: bool,
    pub file: String,
}

#[derive(Debug, Serialize)]
pub struct MusicStatusResponse {
    pub tracks: Vec<MusicTrackStatus>,
    pub count: usize,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct MusicStatusUnavailable;

#[derive(Debug, Clone, Copy, Default, Eq, PartialEq)]
pub struct MusicTrackRequest {
    pub index: Option<usize>,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct MusicTrackBytes {
    pub bytes: Vec<u8>,
    pub content_type: &'static str,
}

#[derive(Debug)]
pub enum MusicTrackError {
    Unavailable,
    NotFound,
    DownloadFailed { body: serde_json::Value },
}

pub async fn music_status(
    state: &AppState,
    handoff: RequestProducerHandoff,
) -> Result<MusicStatusResponse, MusicStatusUnavailable> {
    let producer = handoff.try_claim().map_err(|_| MusicStatusUnavailable)?;
    let owner = state.music_cache().clone();
    let cached = producer
        .spawn_joinable(async move { tokio::task::spawn_blocking(move || owner.status()).await })
        .await
        .map_err(|_| MusicStatusUnavailable)?
        .unwrap_or([false; MUSIC_TRACKS.len()]);
    let tracks = MUSIC_TRACKS
        .iter()
        .enumerate()
        .map(|(index, track)| MusicTrackStatus {
            cached: cached[index],
            file: track.file.to_string(),
        })
        .collect::<Vec<_>>();

    Ok(MusicStatusResponse {
        count: tracks.len(),
        tracks,
    })
}

pub async fn music_track(
    state: &AppState,
    request: MusicTrackRequest,
    handoff: RequestProducerHandoff,
) -> Result<MusicTrackBytes, MusicTrackError> {
    let index = request
        .index
        .unwrap_or(0)
        .min(MUSIC_TRACKS.len().saturating_sub(1));
    let track = MUSIC_TRACKS
        .get(index)
        .copied()
        .ok_or(MusicTrackError::NotFound)?;
    let name = LeafName::new(track.file).expect("fixed music track leaf is valid");
    let worker_name = name.clone();
    let owner = state.music_cache().clone();
    let claim = owner
        .claim_flight(track.id, &handoff, move |owner, track, id, producer| {
            spawn_music_worker(owner, track, id, worker_name, producer);
        })
        .map_err(|_| MusicTrackError::Unavailable)?;

    let completion = match claim {
        MusicFlightClaim::Started(receiver) | MusicFlightClaim::Join(receiver) => {
            wait_for_music_flight(receiver).await
        }
        MusicFlightClaim::Unsettled => MusicFlightCompletion::Unsettled,
    };
    match completion {
        MusicFlightCompletion::Ready => read_track_owned(owner, name, handoff).await,
        MusicFlightCompletion::Failed => Err(download_failed()),
        MusicFlightCompletion::Unsettled => match read_track_owned(owner, name, handoff).await {
            Ok(track) => Ok(track),
            Err(MusicTrackError::Unavailable) => Err(MusicTrackError::Unavailable),
            Err(_) => Err(download_failed()),
        },
        MusicFlightCompletion::Running => Err(download_failed()),
    }
}

fn spawn_music_worker(
    owner: MusicCacheOwner,
    track: MusicTrackId,
    id: u64,
    name: LeafName,
    producer: ProducerLease,
) {
    let shutdown = producer.wait_for_request_drain_start();
    producer.spawn(async move {
        let guard = MusicFlightGuard::new(owner.clone(), track, id);
        let completion = run_music_flight(owner, track, name, shutdown).await;
        guard.finish(completion);
    });
}

async fn run_music_flight(
    owner: MusicCacheOwner,
    track: MusicTrackId,
    name: LeafName,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> MusicFlightCompletion {
    match read_cached_on_blocking(owner.clone(), name.clone()).await {
        BlockingTrackRead::Ready => return MusicFlightCompletion::Ready,
        BlockingTrackRead::Missing => {}
        BlockingTrackRead::Failed => return MusicFlightCompletion::Failed,
        BlockingTrackRead::Unsettled => return MusicFlightCompletion::Unsettled,
    }
    let Some(client) = owner.client() else {
        return MusicFlightCompletion::Failed;
    };
    let Some(source) = owner.source(track) else {
        return MusicFlightCompletion::Failed;
    };
    let target = match prepare_target_on_blocking(owner.clone(), name.clone()).await {
        TargetPreparation::Target(target) => target,
        TargetPreparation::OccupantReady => return MusicFlightCompletion::Ready,
        TargetPreparation::Failed => return MusicFlightCompletion::Failed,
        TargetPreparation::Unsettled => return MusicFlightCompletion::Unsettled,
    };
    let contract = TransferContract::unauthenticated_at_most(
        NonZeroU64::new(MUSIC_MAX_BYTES).expect("music byte limit is nonzero"),
    );
    let (cancellation_sender, cancellation) = transfer_cancellation_channel();
    let transfer = start_create_only_transfer(
        client,
        source,
        target,
        contract,
        RetryPolicy::none(),
        cancellation,
    );
    let joined = transfer.join();
    tokio::pin!(joined);
    tokio::pin!(shutdown);
    let outcome = tokio::select! {
        biased;
        () = &mut shutdown => {
            cancellation_sender.cancel();
            joined.await
        }
        outcome = &mut joined => outcome,
    };
    drop(cancellation_sender);
    settle_transfer_outcome(owner, name, outcome).await
}

async fn settle_transfer_outcome(
    owner: MusicCacheOwner,
    name: LeafName,
    outcome: TransferOutcome<VerifiedCreateOnly>,
) -> MusicFlightCompletion {
    match outcome {
        TransferOutcome::Complete(verified) => {
            tokio::task::spawn_blocking(move || {
                settle_publication(owner, name, verified.publish_create_new())
            })
            .await
            .unwrap_or(MusicFlightCompletion::Unsettled)
        }
        TransferOutcome::Failed { .. } => MusicFlightCompletion::Failed,
        TransferOutcome::CleanupPending(obligation) => tokio::task::spawn_blocking(move || {
            match obligation.reconcile() {
                TransferCleanupResolution::Discarded { .. } => MusicFlightCompletion::Failed,
                TransferCleanupResolution::Pending(obligation) => {
                    drop(obligation);
                    MusicFlightCompletion::Unsettled
                }
            }
        })
        .await
        .unwrap_or(MusicFlightCompletion::Unsettled),
        TransferOutcome::Unsettled { .. } => MusicFlightCompletion::Unsettled,
    }
}

fn settle_publication(
    owner: MusicCacheOwner,
    name: LeafName,
    outcome: TransferPublicationOutcome,
) -> MusicFlightCompletion {
    match outcome {
        TransferPublicationOutcome::Published { file, .. } => owner
            .published_is_bounded(file, &name)
            .map(|ready| {
                if ready {
                    MusicFlightCompletion::Ready
                } else {
                    MusicFlightCompletion::Failed
                }
            })
            .unwrap_or(MusicFlightCompletion::Failed),
        TransferPublicationOutcome::NoEffect { verified, .. } => {
            settle_no_effect_publication(owner, name, verified)
        }
        TransferPublicationOutcome::Pending(obligation) => match obligation.reconcile() {
            TransferPublicationOutcome::Pending(obligation) => {
                drop(obligation);
                MusicFlightCompletion::Unsettled
            }
            outcome => settle_publication(owner, name, outcome),
        },
    }
}

fn settle_no_effect_publication(
    owner: MusicCacheOwner,
    name: LeafName,
    verified: VerifiedCreateOnly,
) -> MusicFlightCompletion {
    match verified.discard() {
        VerifiedTransferDiscardOutcome::Discarded { .. } => {
            recheck_exact_occupant(owner, name)
        }
        VerifiedTransferDiscardOutcome::Pending(obligation) => match obligation.reconcile() {
            VerifiedTransferDiscardOutcome::Discarded { .. } => {
                recheck_exact_occupant(owner, name)
            }
            VerifiedTransferDiscardOutcome::Pending(obligation) => {
                drop(obligation);
                MusicFlightCompletion::Unsettled
            }
        },
    }
}

fn recheck_exact_occupant(owner: MusicCacheOwner, name: LeafName) -> MusicFlightCompletion {
    match owner.cached_track_is_bounded(&name) {
        Ok(true) => MusicFlightCompletion::Ready,
        Ok(false) | Err(_) => MusicFlightCompletion::Failed,
    }
}

enum TargetPreparation {
    Target(CreateOnlyTransferTarget),
    OccupantReady,
    Failed,
    Unsettled,
}

async fn prepare_target_on_blocking(
    owner: MusicCacheOwner,
    name: LeafName,
) -> TargetPreparation {
    tokio::task::spawn_blocking(move || match owner.prepare_target(name.clone()) {
        Ok(target) => TargetPreparation::Target(target),
        Err(_) => match owner.cached_track_is_bounded(&name) {
            Ok(true) => TargetPreparation::OccupantReady,
            Ok(false) | Err(_) => TargetPreparation::Failed,
        },
    })
    .await
    .unwrap_or(TargetPreparation::Unsettled)
}

enum BlockingTrackRead {
    Ready,
    Missing,
    Failed,
    Unsettled,
}

async fn read_cached_on_blocking(
    owner: MusicCacheOwner,
    name: LeafName,
) -> BlockingTrackRead {
    tokio::task::spawn_blocking(move || match owner.cached_track_is_bounded(&name) {
        Ok(true) => BlockingTrackRead::Ready,
        Ok(false) => BlockingTrackRead::Missing,
        Err(_) => BlockingTrackRead::Failed,
    })
    .await
    .unwrap_or(BlockingTrackRead::Unsettled)
}

async fn read_track_owned(
    owner: MusicCacheOwner,
    name: LeafName,
    handoff: RequestProducerHandoff,
) -> Result<MusicTrackBytes, MusicTrackError> {
    let producer = handoff
        .try_claim()
        .map_err(|_| MusicTrackError::Unavailable)?;
    let blocking = producer
        .spawn_joinable(async move {
            tokio::task::spawn_blocking(move || owner.cached_track(&name)).await
        })
        .await
        .map_err(|_| MusicTrackError::NotFound)?;
    let read = blocking.map_err(|_| MusicTrackError::NotFound)?;
    let bytes = read
        .map_err(|_| MusicTrackError::NotFound)?
        .ok_or(MusicTrackError::NotFound)?;
    Ok(MusicTrackBytes {
        bytes,
        content_type: MUSIC_CONTENT_TYPE,
    })
}

async fn wait_for_music_flight(
    mut receiver: tokio::sync::watch::Receiver<MusicFlightCompletion>,
) -> MusicFlightCompletion {
    loop {
        let completion = *receiver.borrow_and_update();
        if completion != MusicFlightCompletion::Running {
            return completion;
        }
        if receiver.changed().await.is_err() {
            return MusicFlightCompletion::Unsettled;
        }
    }
}

struct MusicFlightGuard {
    owner: MusicCacheOwner,
    track: MusicTrackId,
    id: u64,
    armed: bool,
}

impl MusicFlightGuard {
    fn new(owner: MusicCacheOwner, track: MusicTrackId, id: u64) -> Self {
        Self {
            owner,
            track,
            id,
            armed: true,
        }
    }

    fn finish(mut self, completion: MusicFlightCompletion) {
        self.owner.finish_flight(self.track, self.id, completion);
        self.armed = false;
    }
}

impl Drop for MusicFlightGuard {
    fn drop(&mut self) {
        if self.armed {
            self.owner
                .finish_flight(self.track, self.id, MusicFlightCompletion::Unsettled);
        }
    }
}

fn download_failed() -> MusicTrackError {
    MusicTrackError::DownloadFailed {
        body: serde_json::json!({ "error": MUSIC_DOWNLOAD_FAILURE_COPY }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(target_os = "linux")]
    use crate::state::{
        AppStateInit, InstallStore, MusicTestSources, SessionStore,
    };
    #[cfg(target_os = "linux")]
    use axial_config::{AppPaths, ConfigStore, InstanceRegistrySnapshot, InstanceStore};
    #[cfg(target_os = "linux")]
    use axial_performance::PerformanceManager;
    #[cfg(target_os = "linux")]
    use std::path::PathBuf;
    #[cfg(target_os = "linux")]
    use std::sync::Arc;
    #[cfg(target_os = "linux")]
    use std::time::{Duration, SystemTime, UNIX_EPOCH};
    #[cfg(target_os = "linux")]
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn music_failure_copy_is_bounded_and_opaque() {
        let MusicTrackError::DownloadFailed { body } = download_failed() else {
            panic!("expected download failure")
        };
        let encoded = serde_json::to_string(&body).expect("serialize public music error");
        assert_eq!(
            body.get("error").and_then(serde_json::Value::as_str),
            Some(MUSIC_DOWNLOAD_FAILURE_COPY)
        );
        assert!(!encoded.contains("github.com"));
        assert!(!encoded.contains("AppData"));
        assert!(!encoded.contains("/home/"));
    }

    #[test]
    fn music_index_clamps_to_the_fixed_inventory() {
        let index = usize::MAX.min(MUSIC_TRACKS.len().saturating_sub(1));
        assert_eq!(MUSIC_TRACKS[index].id, MusicTrackId::SublunarHum);
        assert_eq!(MUSIC_TRACKS[index].file, "sublunar-hum.mp3");
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn loopback_transfer_publishes_once_then_serves_the_cache_without_http() {
        const BODY: &[u8] = b"bounded loopback music";
        let (source, server) = serve_once(BODY).await;
        let (state, root) = test_state("loopback-cache", source.clone(), source);

        let concurrent = tokio::time::timeout(Duration::from_secs(5), async {
            tokio::join!(
                request_track(state.clone(), 0),
                request_track(state.clone(), 0),
                request_track(state.clone(), 0),
                request_track(state.clone(), 0),
            )
        })
        .await
        .expect("concurrent music requests settle");
        for result in [concurrent.0, concurrent.1, concurrent.2, concurrent.3] {
            assert_eq!(
                result
                    .expect("same-track requests share one transfer")
                    .bytes,
                BODY
            );
        }
        server.await.expect("loopback server task");

        let second = request_track(state.clone(), 0)
            .await
            .expect("second request reads the published cache");
        assert_eq!(second.bytes, BODY);
        assert_eq!(
            std::fs::read(root.join("music").join(MUSIC_TRACKS[0].file))
                .expect("read published music cache"),
            BODY
        );

        drop(state);
        std::fs::remove_dir_all(root).expect("remove music application fixture");
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn music_status_requires_a_live_request_handoff() {
        let source = reqwest::Url::parse("http://127.0.0.1:9/music")
            .expect("fixed loopback test URL");
        let (state, root) = test_state("status-lifecycle", source.clone(), source);
        let request = state.try_admit_request().expect("admit status request");
        let handoff = request.producer_handoff();
        drop(request);

        assert!(matches!(
            music_status(&state, handoff).await,
            Err(MusicStatusUnavailable)
        ));
        drop(state);
        std::fs::remove_dir_all(root).expect("remove music status fixture");
    }

    #[cfg(target_os = "linux")]
    async fn request_track(
        state: AppState,
        index: usize,
    ) -> Result<MusicTrackBytes, MusicTrackError> {
        let request = state.try_admit_request().expect("admit music request");
        let handoff = request.producer_handoff();
        let result = music_track(&state, MusicTrackRequest { index: Some(index) }, handoff).await;
        drop(request);
        result
    }

    #[cfg(target_os = "linux")]
    fn test_state(
        label: &str,
        vapor_halo: reqwest::Url,
        sublunar_hum: reqwest::Url,
    ) -> (AppState, PathBuf) {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be after unix epoch")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "axial-application-music-{label}-{}-{nonce}",
            std::process::id()
        ));
        let paths = AppPaths::from_root(root.clone()).expect("absolute test app root");
        let root_session = crate::state::test_root_session(&paths);
        let music_cache = MusicCacheOwner::with_test_sources(
            Arc::clone(&root_session),
            MusicTestSources {
                vapor_halo,
                sublunar_hum,
            },
        );
        let config = Arc::new(
            ConfigStore::load_from(paths.clone(), Arc::clone(&root_session))
                .expect("load config"),
        );
        let instances = Arc::new(
            InstanceStore::from_snapshot(
                paths.clone(),
                root_session,
                InstanceRegistrySnapshot::default(),
            )
            .expect("load instances"),
        );
        let state = AppState::new(AppStateInit {
            app_name: "Axial".to_string(),
            version: "test".to_string(),
            config,
            instances,
            installs: Arc::new(InstallStore::new()),
            sessions: Arc::new(SessionStore::new()),
            performance: Arc::new(
                PerformanceManager::load_for_startup(paths.performance_dir())
                    .expect("performance manager"),
            ),
            startup_warnings: Vec::new(),
        })
        .with_music_cache(music_cache);
        (state, root)
    }

    #[cfg(target_os = "linux")]
    async fn serve_once(
        body: &'static [u8],
    ) -> (reqwest::Url, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind music transfer fixture");
        let address = listener.local_addr().expect("music fixture address");
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept music request");
            let mut request = [0_u8; 4096];
            let mut used = 0;
            loop {
                let read = stream
                    .read(&mut request[used..])
                    .await
                    .expect("read music request");
                assert!(read > 0, "music request ended before its headers");
                used += read;
                if request[..used].windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
                assert!(used < request.len(), "music request headers exceeded 4 KiB");
            }
            let request = std::str::from_utf8(&request[..used]).expect("HTTP request text");
            assert!(request.starts_with("GET /music HTTP/1.1"));
            assert!(
                request
                    .to_ascii_lowercase()
                    .contains("accept-encoding: identity")
            );
            let headers = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            stream
                .write_all(headers.as_bytes())
                .await
                .expect("write music response headers");
            stream
                .write_all(body)
                .await
                .expect("write music response body");
        });
        (
            reqwest::Url::parse(&format!("http://{address}/music"))
                .expect("music fixture URL"),
            server,
        )
    }
}
