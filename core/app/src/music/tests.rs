use super::*;
use crate::library::LibraryOpenOutcome;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    sync::mpsc,
    task::JoinHandle,
    time::timeout,
};

struct Provider {
    source: reqwest::Url,
    requests: mpsc::UnboundedReceiver<String>,
    replies: mpsc::UnboundedSender<Vec<u8>>,
    task: JoinHandle<()>,
}

impl Provider {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let source =
            reqwest::Url::parse(&format!("http://{}/music", listener.local_addr().unwrap()))
                .unwrap();
        let (sent, requests) = mpsc::unbounded_channel();
        let (replies, mut responses) = mpsc::unbounded_channel::<Vec<u8>>();
        let task = tokio::spawn(async move {
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                    let mut bytes = [0; 1024];
                    let count = stream.read(&mut bytes).await.unwrap();
                    assert!(count > 0);
                    request.extend_from_slice(&bytes[..count]);
                    assert!(request.len() < 8192);
                }
                let request = String::from_utf8(request).unwrap();
                assert!(request.starts_with("GET /music HTTP/1.1"));
                assert!(
                    request
                        .to_ascii_lowercase()
                        .contains("accept-encoding: identity")
                );
                if sent.send(request).is_err() {
                    return;
                }
                let Some(reply) = responses.recv().await else {
                    return;
                };
                let _ = stream.write_all(&reply).await;
                let _ = stream.shutdown().await;
            }
        });
        Self {
            source,
            requests,
            replies,
            task,
        }
    }

    fn reply(&self, status: &str, declared: u64, body: &[u8]) {
        let mut response =
            format!("HTTP/1.1 {status}\r\nContent-Length: {declared}\r\nConnection: close\r\n\r\n")
                .into_bytes();
        response.extend_from_slice(body);
        self.replies.send(response).unwrap();
    }

    async fn requested(&mut self) {
        timeout(Duration::from_secs(5), self.requests.recv())
            .await
            .unwrap()
            .unwrap();
    }

    async fn requested_before(
        &mut self,
        waiter: &mut JoinHandle<Result<MusicTrackBytes, MusicError>>,
    ) {
        tokio::select! {
            _ = self.requested() => {},
            completed = waiter => match completed {
                Ok(Err(error)) => panic!("music stopped before its provider request: {error:?}"),
                Ok(Ok(_)) => panic!("music reported cached success before its provider request"),
                Err(error) => panic!("music waiter failed before its provider request: {error}"),
            },
        }
    }

    async fn idle(&mut self) {
        assert!(
            timeout(Duration::from_millis(30), self.requests.recv())
                .await
                .is_err()
        );
        assert!(!self.task.is_finished());
    }
}

impl Drop for Provider {
    fn drop(&mut self) {
        self.task.abort();
    }
}

struct Fixture {
    music: MusicService,
    library: LibraryLifecycle,
    tasks: TaskOwner,
    temporary: tempfile::TempDir,
}

impl Fixture {
    fn new(source: reqwest::Url) -> Self {
        let temporary =
            tempfile::tempdir_in(std::fs::canonicalize(std::env::temp_dir()).unwrap()).unwrap();
        let library = match LibraryLifecycle::open(temporary.path()) {
            LibraryOpenOutcome::Ready(library) => library,
            other => panic!("isolated fixture root failed: {other:?}"),
        };
        let tasks = TaskOwner::new(8).unwrap();
        let music = local_music(library.clone(), tasks.clone(), source);
        Self {
            music,
            library,
            tasks,
            temporary,
        }
    }

    fn cache_path(&self, index: usize) -> std::path::PathBuf {
        self.temporary.path().join("music").join(MUSIC_FILES[index])
    }

    async fn idle(&self) {
        let mut changes = self.tasks.subscribe();
        timeout(Duration::from_secs(5), async {
            while !self.tasks.status().is_idle() {
                changes.changed().await.unwrap();
            }
        })
        .await
        .unwrap();
    }
}

fn local_music(library: LibraryLifecycle, tasks: TaskOwner, source: reqwest::Url) -> MusicService {
    let origin = TransferOrigin::from_loopback_http_for_test_support(&source).unwrap();
    MusicService::with_sources(library, tasks, [source.clone(), source], vec![origin]).unwrap()
}

#[tokio::test]
async fn missing_cache_admits_and_cancels_an_exact_transient_destination() {
    let provider = Provider::start().await;
    let fixture = Fixture::new(provider.source.clone());
    let pin = fixture.library.admit_application_root().unwrap();
    assert!(cache::read(&pin, 0).unwrap().is_none());
    let directory = cache::prepare_directory(&fixture.music, &pin).unwrap();
    let destination = directory
        .admit_transient_destination(cache::track_name(0))
        .unwrap();
    assert!(matches!(
        destination.cancel(),
        axial_fs::TransientDestinationCancelOutcome::Cancelled
    ));
    assert!(!fixture.cache_path(0).exists());
}

#[tokio::test]
async fn status_retains_inventory_without_creating_cache_or_fetching() {
    let mut provider = Provider::start().await;
    let fixture = Fixture::new(provider.source.clone());
    assert_eq!(
        serde_json::to_value(fixture.music.status().await.unwrap()).unwrap(),
        serde_json::json!({
            "tracks": [{"cached": false, "file": "vapor-halo.mp3"}, {"cached": false, "file": "sublunar-hum.mp3"}], "count": 2,
        })
    );
    assert!(!fixture.temporary.path().join("music").exists());
    provider.idle().await;
}

#[tokio::test]
async fn concurrent_waiters_share_one_real_transfer_then_read_published_cache() {
    let mut provider = Provider::start().await;
    let fixture = Fixture::new(provider.source.clone());
    let mut waiters: Vec<_> = (0..4)
        .map(|_| {
            let music = fixture.music.clone();
            tokio::spawn(async move { music.track(None).await })
        })
        .collect();
    provider.requested_before(&mut waiters[0]).await;
    assert!(!fixture.cache_path(0).exists());
    provider.reply("200 OK", 17, b"music-fixture-v1!");
    for waiter in waiters {
        let result = timeout(Duration::from_secs(5), waiter)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(&*result.bytes, b"music-fixture-v1!");
        assert_eq!(result.content_type, "audio/mpeg");
    }
    fixture.idle().await;
    assert_eq!(
        std::fs::read(fixture.cache_path(0)).unwrap(),
        b"music-fixture-v1!"
    );
    assert_eq!(
        &*fixture.music.track(None).await.unwrap().bytes,
        b"music-fixture-v1!"
    );
    assert!(fixture.music.status().await.unwrap().tracks[0].cached);
    assert!(!fixture.music.has_unsettled_effects());
    provider.idle().await;
}

#[tokio::test]
async fn full_music_limit_publishes_above_metadata_recovery_stage_budget() {
    let mut provider = Provider::start().await;
    let fixture = Fixture::new(provider.source.clone());
    let body = vec![0x5a; MUSIC_MAX_BYTES as usize];
    provider.reply("200 OK", MUSIC_MAX_BYTES, &body);
    let track = timeout(Duration::from_secs(20), fixture.music.track(None))
        .await
        .unwrap()
        .unwrap();
    provider.requested().await;
    assert_eq!(&*track.bytes, body.as_slice());
    assert_eq!(
        std::fs::metadata(fixture.cache_path(0)).unwrap().len(),
        MUSIC_MAX_BYTES
    );
    fixture.idle().await;
    fixture.music.settle().unwrap();
    assert!(!fixture.music.has_unsettled_effects());
    assert_eq!(
        &*fixture.music.track(None).await.unwrap().bytes,
        body.as_slice()
    );
    provider.idle().await;
}

#[tokio::test]
async fn dropping_http_waiter_retains_the_download_until_publication() {
    let mut provider = Provider::start().await;
    let fixture = Fixture::new(provider.source.clone());
    let music = fixture.music.clone();
    let mut waiter = tokio::spawn(async move { music.track(Some(0)).await });
    provider.requested_before(&mut waiter).await;
    waiter.abort();
    assert!(matches!(waiter.await, Err(error) if error.is_cancelled()));
    assert!(!fixture.tasks.status().is_idle());
    provider.reply("200 OK", 8, b"retained");
    fixture.idle().await;
    assert_eq!(std::fs::read(fixture.cache_path(0)).unwrap(), b"retained");
    assert_eq!(
        &*fixture.music.track(None).await.unwrap().bytes,
        b"retained"
    );
    provider.idle().await;
}

#[tokio::test]
async fn shutdown_cancels_and_joins_transfer_without_publishing_partial_music() {
    let mut provider = Provider::start().await;
    let fixture = Fixture::new(provider.source.clone());
    let music = fixture.music.clone();
    let mut waiter = tokio::spawn(async move { music.track(Some(0)).await });
    provider.requested_before(&mut waiter).await;
    fixture
        .tasks
        .shutdown(Duration::from_secs(5))
        .await
        .unwrap();
    assert!(matches!(
        waiter.await.unwrap(),
        Err(MusicError::Unavailable)
    ));
    assert!(!fixture.cache_path(0).exists());
    assert!(!fixture.cache_path(1).exists());
    fixture.music.settle().unwrap();
    assert!(!fixture.music.has_unsettled_effects());
    assert!(fixture.tasks.status().is_idle());
    assert!(matches!(
        fixture.music.track(None).await,
        Err(MusicError::Unavailable)
    ));
    assert!(matches!(
        fixture.music.status().await,
        Err(MusicError::Unavailable)
    ));
}

#[tokio::test]
async fn provider_failure_truncation_and_oversize_never_publish_and_allow_retry() {
    for (status, declared, body) in [
        ("503 Service Unavailable", 7, b"private".as_slice()),
        ("200 OK", 100, b"short".as_slice()),
        ("200 OK", MUSIC_MAX_BYTES + 1, b"".as_slice()),
    ] {
        let mut provider = Provider::start().await;
        let fixture = Fixture::new(provider.source.clone());
        provider.reply(status, declared, body);
        let result = timeout(Duration::from_secs(5), fixture.music.track(None))
            .await
            .unwrap();
        assert!(matches!(result, Err(MusicError::DownloadFailed)));
        provider.requested().await;
        fixture.idle().await;
        assert!(!fixture.cache_path(0).exists());
        fixture.music.settle().unwrap();
        provider.reply("200 OK", 5, b"retry");
        assert_eq!(&*fixture.music.track(None).await.unwrap().bytes, b"retry");
        provider.requested().await;
        assert!(!MusicError::DownloadFailed.to_string().contains("private"));
        assert!(!fixture.music.has_unsettled_effects());
    }
}

#[tokio::test]
async fn cached_tracks_survive_service_restart_and_indices_keep_legacy_clamping() {
    let mut provider = Provider::start().await;
    let fixture = Fixture::new(provider.source.clone());
    std::fs::create_dir(fixture.temporary.path().join("music")).unwrap();
    std::fs::write(fixture.cache_path(0), b"first").unwrap();
    std::fs::write(fixture.cache_path(1), b"second").unwrap();
    assert_eq!(&*fixture.music.track(None).await.unwrap().bytes, b"first");
    assert_eq!(
        &*fixture.music.track(Some(usize::MAX)).await.unwrap().bytes,
        b"second"
    );
    fixture
        .tasks
        .shutdown(Duration::from_secs(5))
        .await
        .unwrap();
    fixture.music.settle().unwrap();
    let replacement_tasks = TaskOwner::new(8).unwrap();
    let restarted = local_music(
        fixture.library.clone(),
        replacement_tasks.clone(),
        provider.source.clone(),
    );
    assert_eq!(&*restarted.track(Some(1)).await.unwrap().bytes, b"second");
    assert!(
        restarted
            .status()
            .await
            .unwrap()
            .tracks
            .iter()
            .all(|track| track.cached)
    );
    replacement_tasks
        .shutdown(Duration::from_secs(5))
        .await
        .unwrap();
    restarted.settle().unwrap();
    provider.idle().await;
}

#[tokio::test]
async fn oversized_and_case_aliased_cache_entries_are_preserved_and_never_served() {
    let mut provider = Provider::start().await;
    let fixture = Fixture::new(provider.source.clone());
    let directory = fixture.temporary.path().join("music");
    std::fs::create_dir(&directory).unwrap();
    let path = fixture.cache_path(0);
    let file = std::fs::File::create(&path).unwrap();
    file.set_len(MUSIC_MAX_BYTES + 1).unwrap();
    assert!(!fixture.music.status().await.unwrap().tracks[0].cached);
    assert!(matches!(
        fixture.music.track(None).await,
        Err(MusicError::DownloadFailed)
    ));
    assert_eq!(std::fs::metadata(&path).unwrap().len(), MUSIC_MAX_BYTES + 1);
    drop(file);
    // Renaming an exact fixture file is a deliberate cache alias, not cleanup.
    std::fs::rename(&path, directory.join("VAPOR-HALO.MP3")).unwrap();
    assert!(!fixture.music.status().await.unwrap().tracks[0].cached);
    assert!(matches!(
        fixture.music.track(None).await,
        Err(MusicError::DownloadFailed)
    ));
    assert!(directory.join("VAPOR-HALO.MP3").exists());
    provider.idle().await;
}

#[cfg(unix)]
#[tokio::test]
async fn symlinked_cache_track_cannot_read_an_external_file() {
    let mut provider = Provider::start().await;
    let fixture = Fixture::new(provider.source.clone());
    let external = tempfile::tempdir().unwrap();
    let private = external.path().join("private.txt");
    std::fs::write(&private, b"private-canary").unwrap();
    std::fs::create_dir(fixture.temporary.path().join("music")).unwrap();
    std::os::unix::fs::symlink(&private, fixture.cache_path(0)).unwrap();
    assert!(!fixture.music.status().await.unwrap().tracks[0].cached);
    assert!(matches!(
        fixture.music.track(None).await,
        Err(MusicError::DownloadFailed)
    ));
    assert_eq!(std::fs::read(&private).unwrap(), b"private-canary");
    provider.idle().await;
}

#[tokio::test]
async fn closing_library_admission_rejects_music_without_creating_cache() {
    let mut provider = Provider::start().await;
    let fixture = Fixture::new(provider.source.clone());
    fixture.library.close_admission();
    assert!(matches!(
        fixture.music.track(None).await,
        Err(MusicError::Unavailable)
    ));
    assert!(matches!(
        fixture.music.status().await,
        Err(MusicError::Unavailable)
    ));
    assert!(!fixture.temporary.path().join("music").exists());
    provider.idle().await;
}
